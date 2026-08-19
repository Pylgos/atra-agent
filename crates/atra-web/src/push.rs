use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use web_push_native::{
    Auth, WebPushBuilder,
    p256::{
        PublicKey, SecretKey,
        ecdsa::{Signature, SigningKey, signature::Signer as _},
        elliptic_curve::{rand_core::OsRng, sec1::ToEncodedPoint},
    },
};

const VAPID_CONTACT: &str = "mailto:atra@localhost";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PushSubscription {
    pub endpoint: String,
    pub keys: PushSubscriptionKeys,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PushSubscriptionKeys {
    pub auth: String,
    pub p256dh: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PushTestRequest {
    pub endpoint: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PushPayload {
    pub title: String,
    pub body: String,
    pub tag: String,
    pub url: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedPushState {
    private_key: String,
    subscriptions: Vec<PushSubscription>,
}

#[derive(Clone)]
pub(crate) struct PushManager {
    path: PathBuf,
    state: Arc<Mutex<PersistedPushState>>,
    client: reqwest::Client,
}

impl PushManager {
    pub(crate) fn open(path: PathBuf) -> Result<Self> {
        let state = if path.exists() {
            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to decode {}", path.display()))?
        } else {
            let secret = SecretKey::random(&mut OsRng);
            PersistedPushState {
                private_key: URL_SAFE_NO_PAD.encode(secret.to_bytes()),
                subscriptions: Vec::new(),
            }
        };
        validate_private_key(&state.private_key)?;
        for subscription in &state.subscriptions {
            validate_subscription(subscription)?;
        }
        persist(&path, &state)?;
        Ok(Self {
            path,
            state: Arc::new(Mutex::new(state)),
            client: reqwest::Client::new(),
        })
    }

    pub(crate) async fn public_key(&self) -> Result<String> {
        let state = self.state.lock().await;
        let secret = decode_private_key(&state.private_key)?;
        Ok(URL_SAFE_NO_PAD.encode(secret.public_key().to_encoded_point(false).as_bytes()))
    }

    pub(crate) async fn subscribe(&self, subscription: PushSubscription) -> Result<()> {
        validate_subscription(&subscription)?;
        let mut state = self.state.lock().await;
        if let Some(current) = state
            .subscriptions
            .iter_mut()
            .find(|current| current.endpoint == subscription.endpoint)
        {
            *current = subscription;
        } else {
            state.subscriptions.push(subscription);
        }
        persist(&self.path, &state)
    }

    pub(crate) async fn unsubscribe(&self, endpoint: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        state
            .subscriptions
            .retain(|subscription| subscription.endpoint != endpoint);
        persist(&self.path, &state)
    }

    pub(crate) async fn send_all(&self, payload: &PushPayload) {
        let (private_key, subscriptions) = {
            let state = self.state.lock().await;
            (state.private_key.clone(), state.subscriptions.clone())
        };
        let mut expired = Vec::new();
        for subscription in subscriptions {
            match self.send_one(&private_key, &subscription, payload).await {
                Ok(true) => expired.push(subscription.endpoint),
                Ok(false) => {}
                Err(error) => eprintln!("atra-web: failed to send Web Push: {error:#}"),
            }
        }
        self.remove_expired(&expired).await;
    }

    pub(crate) async fn send_test(&self, endpoint: &str) -> Result<()> {
        let (private_key, subscription) = {
            let state = self.state.lock().await;
            let subscription = state
                .subscriptions
                .iter()
                .find(|subscription| subscription.endpoint == endpoint)
                .cloned()
                .context("this browser is not subscribed")?;
            (state.private_key.clone(), subscription)
        };
        let payload = PushPayload {
            title: "Atra test notification".to_owned(),
            body: "Web Push is working.".to_owned(),
            tag: "atra-test".to_owned(),
            url: "/".to_owned(),
        };
        if self.send_one(&private_key, &subscription, &payload).await? {
            self.remove_expired(&[endpoint.to_owned()]).await;
            bail!("the browser Push subscription has expired");
        }
        Ok(())
    }

    async fn send_one(
        &self,
        private_key: &str,
        subscription: &PushSubscription,
        payload: &PushPayload,
    ) -> Result<bool> {
        let secret = decode_private_key(private_key)?;
        let public = PublicKey::from_sec1_bytes(
            &URL_SAFE_NO_PAD
                .decode(&subscription.keys.p256dh)
                .context("invalid p256dh key")?,
        )
        .context("invalid p256dh key")?;
        let auth = URL_SAFE_NO_PAD
            .decode(&subscription.keys.auth)
            .context("invalid auth key")?;
        let auth = Auth::clone_from_slice(&auth);
        let mut request = WebPushBuilder::new(
            subscription
                .endpoint
                .parse()
                .context("invalid Push endpoint")?,
            public,
            auth,
        )
        .build(serde_json::to_vec(payload).context("failed to encode Push payload")?)
        .context("failed to build Web Push request")?;
        let authorization = vapid_authorization(&secret, &subscription.endpoint)?;
        request.headers_mut().insert(
            "Authorization",
            authorization
                .parse()
                .context("failed to build VAPID authorization")?,
        );
        let (parts, body) = request.into_parts();
        let mut outgoing = self.client.post(parts.uri.to_string());
        for (name, value) in &parts.headers {
            outgoing = outgoing.header(name.as_str(), value.as_bytes());
        }
        let response = outgoing
            .body(body)
            .send()
            .await
            .context("Push service request failed")?;
        if response.status().is_success() {
            return Ok(false);
        }
        if matches!(response.status(), StatusCode::NOT_FOUND | StatusCode::GONE) {
            return Ok(true);
        }
        bail!("Push service returned {}", response.status())
    }

    async fn remove_expired(&self, endpoints: &[String]) {
        if endpoints.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        state
            .subscriptions
            .retain(|subscription| !endpoints.contains(&subscription.endpoint));
        if let Err(error) = persist(&self.path, &state) {
            eprintln!("atra-web: failed to remove expired Web Push subscriptions: {error:#}");
        }
    }
}

fn vapid_authorization(secret: &SecretKey, endpoint: &str) -> Result<String> {
    let endpoint = reqwest::Url::parse(endpoint).context("invalid Push endpoint")?;
    let audience = endpoint.origin().ascii_serialization();
    let expires = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs()
        + 12 * 60 * 60;
    let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({
        "aud": audience,
        "exp": expires,
        "sub": VAPID_CONTACT,
    }))?);
    let signing_input = format!("{header}.{claims}");
    let signing_key = SigningKey::from(secret.clone());
    let signature: Signature = signing_key.sign(signing_input.as_bytes());
    let token = format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    );
    let public_key = URL_SAFE_NO_PAD.encode(secret.public_key().to_encoded_point(false).as_bytes());
    Ok(format!("vapid t={token}, k={public_key}"))
}

fn decode_private_key(encoded: &str) -> Result<SecretKey> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .context("invalid persisted VAPID key")?;
    SecretKey::from_slice(&bytes).context("invalid persisted VAPID key")
}

fn validate_private_key(encoded: &str) -> Result<()> {
    decode_private_key(encoded).map(|_| ())
}

fn validate_subscription(subscription: &PushSubscription) -> Result<()> {
    let endpoint = reqwest::Url::parse(&subscription.endpoint).context("invalid Push endpoint")?;
    if endpoint.scheme() != "https" || subscription.endpoint.len() > 4096 {
        bail!("Push endpoint must be a reasonably sized HTTPS URL");
    }
    let p256dh = URL_SAFE_NO_PAD
        .decode(&subscription.keys.p256dh)
        .context("invalid p256dh key")?;
    PublicKey::from_sec1_bytes(&p256dh).context("invalid p256dh key")?;
    let auth = URL_SAFE_NO_PAD
        .decode(&subscription.keys.auth)
        .context("invalid auth key")?;
    if auth.len() != 16 {
        bail!("invalid auth key");
    }
    Ok(())
}

fn persist(path: &Path, state: &PersistedPushState) -> Result<()> {
    let parent = path.parent().context("Push state path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure {}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(state).context("failed to encode Push state")?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn vapid_key_survives_reopen() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("push.json");
        let first = PushManager::open(path.clone())
            .unwrap()
            .public_key()
            .await
            .unwrap();
        let second = PushManager::open(path).unwrap().public_key().await.unwrap();
        assert_eq!(first, second);
    }
}
