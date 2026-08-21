use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use reqwest::{
    Client, StatusCode, Url,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, RwLock},
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const CALLBACK_PORTS: [u16; 2] = [1455, 1457];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AuthFile {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY", default)]
    openai_api_key: Option<String>,
    tokens: Option<Tokens>,
    #[serde(default)]
    last_refresh: Option<Value>,
}

#[derive(Clone)]
pub(super) struct Auth {
    pub token: String,
    pub account_id: Option<String>,
    pub email: Option<String>,
}

pub(super) struct AuthManager {
    home: PathBuf,
    client: Client,
    tokens: RwLock<Option<Tokens>>,
    refresh: Mutex<()>,
}

impl AuthManager {
    pub(super) fn credential_source(&self) -> Option<atra_protocol::CredentialSource> {
        self.home
            .join("auth.json")
            .is_file()
            .then_some(atra_protocol::CredentialSource::File)
    }

    pub(super) async fn new(home: PathBuf) -> Self {
        let tokens = match read_tokens(&home) {
            Ok(tokens) => tokens,
            Err(error) => {
                tracing::warn!(%error, path = %home.join("auth.json").display(), "failed to load Codex login");
                None
            }
        };
        Self {
            home,
            client: default_client(),
            tokens: RwLock::new(tokens),
            refresh: Mutex::new(()),
        }
    }

    pub(super) async fn reload(&self) -> Result<()> {
        *self.tokens.write().await = read_tokens(&self.home)?;
        Ok(())
    }

    pub(super) async fn auth(&self) -> Result<Option<Auth>> {
        let needs_refresh = self
            .tokens
            .read()
            .await
            .as_ref()
            .is_some_and(|tokens| token_expires_soon(&tokens.access_token));
        if needs_refresh {
            self.refresh().await?;
        }
        Ok(self.tokens.read().await.as_ref().map(auth_from_tokens))
    }

    pub(super) async fn recover_unauthorized(&self, previous_token: &str) -> Result<Option<Auth>> {
        let _guard = self.refresh.lock().await;
        let disk = read_tokens(&self.home)?;
        if disk
            .as_ref()
            .is_some_and(|tokens| tokens.access_token != previous_token)
        {
            *self.tokens.write().await = disk;
        } else {
            self.refresh_locked().await?;
        }
        Ok(self.tokens.read().await.as_ref().map(auth_from_tokens))
    }

    async fn refresh(&self) -> Result<()> {
        let _guard = self.refresh.lock().await;
        if self
            .tokens
            .read()
            .await
            .as_ref()
            .is_some_and(|tokens| !token_expires_soon(&tokens.access_token))
        {
            return Ok(());
        }
        self.refresh_locked().await
    }

    async fn refresh_locked(&self) -> Result<()> {
        let current = self
            .tokens
            .read()
            .await
            .clone()
            .context("Codex login required; run `atra provider login codex`")?;
        #[derive(Deserialize)]
        struct Response {
            id_token: Option<String>,
            access_token: Option<String>,
            refresh_token: Option<String>,
        }
        let endpoint = std::env::var("CODEX_REFRESH_TOKEN_URL_OVERRIDE")
            .unwrap_or_else(|_| format!("{ISSUER}/oauth/token"));
        let response = self
            .client
            .post(endpoint)
            .json(&serde_json::json!({
                "client_id": oauth_client_id(),
                "grant_type": "refresh_token",
                "refresh_token": current.refresh_token,
            }))
            .send()
            .await
            .context("failed to refresh Codex login")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read token refresh response")?;
        if !status.is_success() {
            bail!(
                "Codex token refresh failed ({status}): {}",
                error_message(&body)
            );
        }
        let refreshed: Response =
            serde_json::from_str(&body).context("invalid Codex token refresh response")?;
        let tokens = Tokens {
            id_token: refreshed.id_token.unwrap_or(current.id_token),
            access_token: refreshed.access_token.unwrap_or(current.access_token),
            refresh_token: refreshed.refresh_token.unwrap_or(current.refresh_token),
            account_id: current.account_id,
        };
        write_tokens(&self.home, &tokens)?;
        *self.tokens.write().await = Some(tokens);
        Ok(())
    }
}

pub(crate) async fn login(home: &Path) -> Result<()> {
    prepare_home(home)?;
    let listener = bind_callback().await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let verifier = random_urlsafe(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(32);
    let client_id = oauth_client_id();
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair(
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        )
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", &state)
        .append_pair("originator", &originator());
    eprintln!("Open this URL to sign in:\n{url}");
    let _ = webbrowser::open(url.as_str());

    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut bytes = vec![0; 16 * 1024];
        let count = socket.read(&mut bytes).await?;
        let request = String::from_utf8_lossy(&bytes[..count]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let parsed = Url::parse(&format!("http://localhost{target}"))?;
        if parsed.path() != "/auth/callback" {
            respond(&mut socket, StatusCode::NOT_FOUND, "Not found").await?;
            continue;
        }
        let params = parsed
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        if params.get("state") != Some(&state) {
            respond(&mut socket, StatusCode::BAD_REQUEST, "State mismatch").await?;
            continue;
        }
        if let Some(error) = params.get("error") {
            let description = params
                .get("error_description")
                .map(String::as_str)
                .unwrap_or(error);
            respond(&mut socket, StatusCode::BAD_REQUEST, description).await?;
            bail!("Codex login failed: {description}");
        }
        let code = params.get("code").filter(|code| !code.is_empty()).cloned();
        if let Some(code) = code {
            match exchange_code(home, &code, &redirect_uri, &client_id, &verifier).await {
                Ok(()) => {
                    respond(
                        &mut socket,
                        StatusCode::OK,
                        "Sign-in complete. You can close this window.",
                    )
                    .await?;
                    return Ok(());
                }
                Err(error) => {
                    let _ = respond(
                        &mut socket,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Sign-in failed. Return to the terminal for details.",
                    )
                    .await;
                    return Err(error);
                }
            }
        }
        respond(
            &mut socket,
            StatusCode::BAD_REQUEST,
            "Missing authorization code",
        )
        .await?;
    }
}

async fn exchange_code(
    home: &Path,
    code: &str,
    redirect_uri: &str,
    client_id: &str,
    verifier: &str,
) -> Result<()> {
    #[derive(Deserialize)]
    struct Exchange {
        id_token: String,
        access_token: String,
        refresh_token: String,
    }
    let response = default_client()
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("failed to exchange Codex authorization code")?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        bail!(
            "Codex token exchange failed ({status}): {}",
            error_message(&body)
        );
    }
    let exchange: Exchange =
        serde_json::from_str(&body).context("invalid Codex token exchange response")?;
    let claims = jwt_payload(&exchange.id_token).unwrap_or(Value::Null);
    let account_id = claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    write_tokens(
        home,
        &Tokens {
            id_token: exchange.id_token,
            access_token: exchange.access_token,
            refresh_token: exchange.refresh_token,
            account_id,
        },
    )
}

pub(crate) async fn logout(home: &Path) -> Result<()> {
    let tokens = match read_tokens(home) {
        Ok(tokens) => tokens,
        Err(error) => {
            tracing::warn!(%error, path = %home.join("auth.json").display(), "failed to load Codex login during logout");
            None
        }
    };
    if let Some(tokens) = tokens {
        let endpoint = std::env::var("CODEX_REVOKE_TOKEN_URL_OVERRIDE")
            .unwrap_or_else(|_| format!("{ISSUER}/oauth/revoke"));
        let (token, hint, client_id) = if tokens.refresh_token.is_empty() {
            (&tokens.access_token, "access_token", None)
        } else {
            (
                &tokens.refresh_token,
                "refresh_token",
                Some(oauth_client_id()),
            )
        };
        let _ = default_client()
            .post(endpoint)
            .timeout(std::time::Duration::from_secs(10))
            .json(&serde_json::json!({
                "token": token,
                "token_type_hint": hint,
                "client_id": client_id,
            }))
            .send()
            .await;
    }
    match fs::remove_file(home.join("auth.json")) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to remove Codex login"),
    }
}

fn auth_from_tokens(tokens: &Tokens) -> Auth {
    let claims = jwt_payload(&tokens.id_token).unwrap_or(Value::Null);
    let email = claims
        .get("email")
        .or_else(|| claims.pointer("/https:~1~1api.openai.com~1profile/email"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Auth {
        token: tokens.access_token.clone(),
        account_id: tokens.account_id.clone().or_else(|| {
            claims
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }),
        email,
    }
}

fn read_tokens(home: &Path) -> Result<Option<Tokens>> {
    let path = home.join("auth.json");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let auth: AuthFile = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(auth.tokens)
}

fn write_tokens(home: &Path, tokens: &Tokens) -> Result<()> {
    prepare_home(home)?;
    let path = home.join("auth.json");
    let temporary = home.join(format!(
        ".auth.json.{}.{}.tmp",
        std::process::id(),
        random_urlsafe(12)
    ));
    let data = serde_json::to_vec_pretty(&AuthFile {
        auth_mode: Some("chatgpt".to_owned()),
        openai_api_key: Some(tokens.access_token.clone()),
        tokens: Some(tokens.clone()),
        last_refresh: Some(Value::String(chrono::Utc::now().to_rfc3339())),
    })?;
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true).mode(0o600);
    let result = (|| -> Result<()> {
        use std::io::Write;

        let mut file = options
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        fs::File::open(home)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn prepare_home(home: &Path) -> Result<()> {
    fs::create_dir_all(home)?;
    fs::set_permissions(home, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

async fn bind_callback() -> Result<TcpListener> {
    for port in CALLBACK_PORTS {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok(listener);
        }
    }
    bail!("Codex login callback ports 1455 and 1457 are both in use")
}

async fn respond(socket: &mut tokio::net::TcpStream, status: StatusCode, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        body.len(),
        body
    );
    socket.write_all(response.as_bytes()).await?;
    Ok(())
}

fn random_urlsafe(len: usize) -> String {
    let mut bytes = vec![0; len];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn oauth_client_id() -> String {
    std::env::var("CODEX_APP_SERVER_LOGIN_CLIENT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| CLIENT_ID.to_owned())
}

pub(super) fn default_client() -> Client {
    let originator = originator();
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&originator) {
        headers.insert("originator", value);
    } else {
        headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
    }
    if let Ok(value) = HeaderValue::from_str(&codex_user_agent(&originator)) {
        headers.insert(USER_AGENT, value);
    }
    Client::builder()
        .default_headers(headers)
        .cookie_store(true)
        .build()
        .expect("Codex HTTP client configuration is valid")
}

fn originator() -> String {
    std::env::var("CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
        .ok()
        .filter(|value| HeaderValue::from_str(value).is_ok())
        .unwrap_or_else(|| "codex_cli_rs".to_owned())
}

fn codex_user_agent(originator: &str) -> String {
    let os = os_info::get();
    let terminal = std::env::var("TERM_PROGRAM")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    format!(
        "{originator}/0.0.0 ({} {}; {}) {terminal}",
        os.os_type(),
        os.version(),
        std::env::consts::ARCH
    )
}

fn jwt_payload(jwt: &str) -> Result<Value> {
    let payload = jwt.split('.').nth(1).context("invalid JWT")?;
    Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload)?)?)
}

fn token_expires_soon(jwt: &str) -> bool {
    let expiration = jwt_payload(jwt)
        .ok()
        .and_then(|claims| claims.get("exp").and_then(Value::as_i64));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    expiration.is_some_and(|expiration| expiration <= now + 300)
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|body| {
            body.get("error_description")
                .or_else(|| body.pointer("/error/message"))
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(500).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn malformed_auth_does_not_prevent_manager_creation() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("auth.json"), "{not json").unwrap();

        let manager = AuthManager::new(home.path().to_owned()).await;

        assert!(manager.auth().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn logout_removes_malformed_auth() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("auth.json");
        fs::write(&path, "{not json").unwrap();

        logout(home.path()).await.unwrap();

        assert!(!path.exists());
    }
}
