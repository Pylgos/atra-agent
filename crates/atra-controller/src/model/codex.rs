use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue},
};

use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};

use atra_protocol::Model;

use super::{
    ModelEventStream, ModelRequest, ProviderLoginStatus, ProviderRuntime,
    codex_auth::{Auth, AuthManager},
};

const PROVIDER_ID: &str = super::CODEX_PROVIDER;
const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
const BUNDLED_MODELS: &str = r#"[
  {
    "slug": "gpt-5.6-sol",
    "display_name": "GPT-5.6-Sol",
    "description": "Latest frontier agentic coding model.",
    "default_reasoning_level": "low",
    "supported_reasoning_levels": ["low", "medium", "high", "xhigh", "max", "ultra"],
    "context_window": 272000
  },
  {
    "slug": "gpt-5.6-terra",
    "display_name": "GPT-5.6-Terra",
    "description": "Balanced agentic coding model for everyday work.",
    "default_reasoning_level": "medium",
    "supported_reasoning_levels": ["low", "medium", "high", "xhigh", "max", "ultra"],
    "context_window": 272000
  },
  {
    "slug": "gpt-5.6-luna",
    "display_name": "GPT-5.6-Luna",
    "description": "Fast and affordable agentic coding model.",
    "default_reasoning_level": "medium",
    "supported_reasoning_levels": ["low", "medium", "high", "xhigh", "max"],
    "context_window": 272000
  },
  {
    "slug": "gpt-5.5",
    "display_name": "GPT-5.5",
    "description": "Frontier model for complex coding, research, and real-world work.",
    "default_reasoning_level": "medium",
    "supported_reasoning_levels": ["low", "medium", "high", "xhigh"],
    "context_window": 272000
  },
  {
    "slug": "gpt-5.2",
    "display_name": "GPT-5.2",
    "description": "Optimized for professional work and long-running agents.",
    "default_reasoning_level": "medium",
    "supported_reasoning_levels": ["low", "medium", "high", "xhigh"],
    "context_window": 272000
  }
]"#;

pub(crate) struct CodexProvider {
    auth_home: PathBuf,
    auth: Arc<AuthManager>,
    client: Client,
    sessions: Mutex<HashMap<String, Arc<CodexSession>>>,
    models: RwLock<Option<Vec<Model>>>,
    rate_limits: Arc<RwLock<Vec<Value>>>,
}

struct CodexSession {
    auth: Arc<AuthManager>,
    client: Client,
    session_id: String,
    rate_limits: Arc<RwLock<Vec<Value>>>,
    last_used: std::sync::Mutex<Instant>,
}

impl CodexProvider {
    pub(super) async fn new(auth_home: PathBuf) -> Self {
        Self {
            auth: Arc::new(AuthManager::new(auth_home.clone()).await),
            auth_home,
            client: super::codex_auth::default_client(),
            sessions: Mutex::new(HashMap::new()),
            models: RwLock::new(None),
            rate_limits: Arc::new(RwLock::new(Vec::new())),
        }
    }

    async fn login_status_inner(&self) -> Result<Option<Option<String>>> {
        Ok(self.auth.auth().await?.map(|auth| auth.email))
    }

    async fn reload_auth_inner(&self) -> Result<()> {
        self.auth.reload().await?;
        *self.models.write().await = None;
        self.sessions.lock().await.clear();
        self.rate_limits.write().await.clear();
        Ok(())
    }

    async fn logout_inner(&self) -> Result<()> {
        super::codex_auth::logout(&self.auth_home).await?;
        self.reload_auth_inner().await
    }

    async fn session(&self, session_id: &str) -> Result<Arc<CodexSession>> {
        anyhow::ensure!(
            self.auth.auth().await?.is_some(),
            "Codex login required; run `atra provider login codex`"
        );
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, session| session.idle_for() < SESSION_IDLE_TTL);
        let session = sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                Arc::new(CodexSession {
                    auth: self.auth.clone(),
                    client: self.client.clone(),
                    session_id: session_id.to_owned(),
                    rate_limits: self.rate_limits.clone(),
                    last_used: std::sync::Mutex::new(Instant::now()),
                })
            })
            .clone();
        session.touch();
        Ok(session)
    }

    async fn rate_limits_inner(&self) -> Result<Vec<Value>> {
        let auth = self
            .auth
            .auth()
            .await?
            .context("Codex login required; run `atra provider login codex`")?;
        let response = send_authenticated(
            &self.client,
            &self.auth,
            auth,
            reqwest::Method::GET,
            "https://chatgpt.com/backend-api/wham/usage",
            None,
            HeaderMap::new(),
        )
        .await
        .context("failed to fetch Codex rate limits")?;
        let value = decode_json(response, "Codex rate limits").await?;
        let snapshots = rate_limit_snapshots(&value);
        *self.rate_limits.write().await = snapshots.clone();
        Ok(snapshots)
    }

    async fn models_inner(&self) -> Result<Vec<Model>> {
        if let Some(models) = self.models.read().await.clone() {
            return Ok(models);
        }
        let auth = self
            .auth
            .auth()
            .await?
            .context("Codex login required; run `atra provider login codex`")?;
        let response = send_authenticated(
            &self.client,
            &self.auth,
            auth,
            reqwest::Method::GET,
            &format!("{BASE_URL}/models?client_version=0.0.0"),
            None,
            HeaderMap::new(),
        )
        .await
        .context("failed to list Codex models")?;
        let value = decode_json(response, "Codex models").await?;
        let raw = value
            .get("models")
            .and_then(Value::as_array)
            .context("Codex models response has no models array")?;
        let has_listed = raw.iter().any(|model| model["visibility"] == "list");
        let bundled;
        let raw = if has_listed {
            raw
        } else {
            bundled = serde_json::from_str::<Vec<Value>>(BUNDLED_MODELS)
                .context("failed to load bundled Codex model catalog")?;
            &bundled
        };
        let mut models = raw
            .iter()
            .filter(|model| !has_listed || model["visibility"] == "list")
            .filter_map(parse_model)
            .collect::<Vec<_>>();
        if models.is_empty() {
            models.push(fallback_model());
        }
        *self.models.write().await = Some(models.clone());
        Ok(models)
    }
}

#[async_trait]
impl ProviderRuntime for CodexProvider {
    fn compaction_replay_key(&self, model: &str) -> Option<String> {
        Some(super::compaction_replay_key("codex", model))
    }

    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn auth_method(&self) -> atra_protocol::ProviderAuthMethod {
        atra_protocol::ProviderAuthMethod::Browser
    }

    fn credential_source(&self) -> Option<atra_protocol::CredentialSource> {
        self.auth.credential_source()
    }

    async fn models(&self) -> Result<Vec<Model>> {
        self.models_inner().await
    }

    async fn login(&self, credential: Option<String>) -> Result<ProviderLoginStatus> {
        anyhow::ensure!(
            credential.is_none(),
            "Codex login does not accept a credential"
        );
        super::codex_auth::login(&self.auth_home).await?;
        self.reload_auth_inner().await?;
        self.login_status().await
    }

    async fn login_status(&self) -> Result<ProviderLoginStatus> {
        Ok(match self.login_status_inner().await? {
            Some(account) => ProviderLoginStatus::LoggedIn(account),
            None => ProviderLoginStatus::LoginRequired,
        })
    }

    async fn reload_auth(&self) -> Result<()> {
        self.reload_auth_inner().await
    }

    async fn logout(&self) -> Result<()> {
        self.logout_inner().await
    }

    async fn rate_limits(&self) -> Result<Value> {
        Ok(Value::Array(self.rate_limits_inner().await?))
    }

    async fn execute_tool(
        &self,
        _model: &str,
        _name: &str,
        _arguments: &Value,
    ) -> Result<Option<Value>> {
        Ok(None)
    }

    async fn stream(
        &self,
        session_id: &str,
        request: &ModelRequest<'_>,
    ) -> Result<ModelEventStream> {
        stream(self.session(session_id).await?, request).await
    }

    async fn server_compact(
        &self,
        session_id: &str,
        request: &ModelRequest<'_>,
    ) -> Result<Option<atra_protocol::OpaqueState>> {
        server_compact(self.session(session_id).await?, request)
            .await
            .map(Some)
    }
}

async fn stream(
    session: Arc<CodexSession>,
    request: &ModelRequest<'_>,
) -> Result<ModelEventStream> {
    session.touch();
    let turn_state = Arc::new(Mutex::new(None));
    let body = super::api::responses::request_body(request, super::api::responses::Profile::Codex)?;
    let model = request.model.to_owned();
    Ok(super::api::responses::decode(
        move || {
            let session = Arc::clone(&session);
            let turn_state = Arc::clone(&turn_state);
            let body = body.clone();
            async move {
                let response = session
                    .send(
                        reqwest::Method::POST,
                        &format!("{BASE_URL}/responses"),
                        Some(body),
                        &turn_state,
                    )
                    .await?;
                let rate_limits = rate_limit_headers(response.headers());
                *session.rate_limits.write().await = rate_limits.clone();
                Ok((response, rate_limits))
            }
        },
        model,
        "codex".to_owned(),
    ))
}

async fn server_compact(
    session: Arc<CodexSession>,
    request: &ModelRequest<'_>,
) -> Result<atra_protocol::OpaqueState> {
    session.touch();
    let turn_state = Arc::new(Mutex::new(None));
    let body = super::api::responses::server_compaction_body(request)?;
    let response = tokio::time::timeout(
        REQUEST_TIMEOUT,
        session.send(
            reqwest::Method::POST,
            &format!("{BASE_URL}/responses"),
            Some(body),
            &turn_state,
        ),
    )
    .await
    .context("Codex compaction request timed out")??;
    let payload = super::api::responses::decode_server_compaction(response).await?;
    Ok(atra_protocol::OpaqueState {
        replay_key: super::compaction_replay_key("codex", request.model),
        payload,
    })
}

impl CodexSession {
    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used.lock().unwrap().elapsed()
    }

    async fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Value>,
        turn_state: &Mutex<Option<String>>,
    ) -> Result<Response> {
        let auth = self
            .auth
            .auth()
            .await?
            .context("Codex login required; run `atra provider login codex`")?;
        let mut headers = HeaderMap::new();
        for name in ["session-id", "thread-id", "x-client-request-id"] {
            headers.insert(
                reqwest::header::HeaderName::from_static(name),
                self.session_id
                    .parse()
                    .context("invalid model session ID")?,
            );
        }
        if let Some(turn_state) = turn_state.lock().await.as_ref() {
            headers.insert(
                "x-codex-turn-state",
                turn_state.parse().context("invalid Codex turn state")?,
            );
        }
        headers.insert(
            "x-codex-beta-features",
            HeaderValue::from_static("remote_compaction_v2"),
        );
        let response =
            send_authenticated(&self.client, &self.auth, auth, method, url, body, headers).await?;
        if let Some(next_turn_state) = response
            .headers()
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok())
        {
            *turn_state.lock().await = Some(next_turn_state.to_owned());
        }
        Ok(response)
    }
}

async fn send_authenticated(
    client: &Client,
    manager: &AuthManager,
    mut auth: Auth,
    method: reqwest::Method,
    url: &str,
    body: Option<Value>,
    headers: HeaderMap,
) -> Result<Response> {
    for attempt in 0..2 {
        let mut request = client
            .request(method.clone(), url)
            .headers(headers.clone())
            .bearer_auth(&auth.token);
        if url.starts_with(BASE_URL) {
            request = request.header("version", "0.0.0");
        }
        if let Some(account_id) = &auth.account_id {
            request = request.header("ChatGPT-Account-ID", account_id);
        }
        if let Some(body) = &body {
            request = request.json(body);
        }
        let response = request.send().await.context("Codex request failed")?;
        if response.status() != StatusCode::UNAUTHORIZED || attempt == 1 {
            return Ok(response);
        }
        auth = manager
            .recover_unauthorized(&auth.token)
            .await?
            .context("Codex login required; run `atra provider login codex`")?;
    }
    unreachable!()
}

fn parse_model(value: &Value) -> Option<Model> {
    let id = value["slug"].as_str()?.to_owned();
    let default = value["default_reasoning_level"]
        .as_str()
        .unwrap_or("medium")
        .to_owned();
    let mut supported = value["supported_reasoning_levels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|preset| preset["effort"].as_str().or_else(|| preset.as_str()))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if supported.is_empty() {
        supported.push(default.clone());
    }
    let context_window = value["context_window"].as_i64()?;
    let auto_compact_token_limit = value["auto_compact_token_limit"]
        .as_i64()
        .or_else(|| Some(context_window.saturating_mul(9) / 10));
    Some(Model {
        provider: PROVIDER_ID.to_owned(),
        id: id.clone(),
        display_name: value["display_name"].as_str().unwrap_or(&id).to_owned(),
        description: value["description"].as_str().map(str::to_owned),
        default_reasoning_effort: default,
        supported_reasoning_efforts: supported,
        context_window: Some(context_window),
        auto_compact_token_limit,
        tool_bindings: vec![atra_protocol::ModelToolBinding {
            tool: "web_search".to_owned(),
            implementation: super::tool_binding::codex("web_search")
                .expect("Codex web search binding")
                .name()
                .to_owned(),
        }],
    })
}

fn fallback_model() -> Model {
    Model {
        provider: PROVIDER_ID.to_owned(),
        id: super::DEFAULT_MODEL.to_owned(),
        display_name: super::DEFAULT_MODEL.to_owned(),
        description: Some("Codex".to_owned()),
        default_reasoning_effort: "medium".to_owned(),
        supported_reasoning_efforts: vec!["low", "medium", "high", "xhigh"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        context_window: Some(400_000),
        auto_compact_token_limit: Some(360_000),
        tool_bindings: vec![atra_protocol::ModelToolBinding {
            tool: "web_search".to_owned(),
            implementation: super::tool_binding::codex("web_search")
                .expect("Codex web search binding")
                .name()
                .to_owned(),
        }],
    }
}

async fn decode_json(response: Response, label: &str) -> Result<Value> {
    if !response.status().is_success() {
        return Err(response_error(response, label).await);
    }
    response
        .json()
        .await
        .with_context(|| format!("failed to decode {label}"))
}

async fn response_error(response: Response, label: &str) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow::anyhow!("{label} failed ({status}): {}", error_message(&body))
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("detail"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(500).collect())
}

fn rate_limit_snapshots(value: &Value) -> Vec<Value> {
    if let Some(array) = value.as_array() {
        return array
            .iter()
            .map(|limit| {
                let id = limit["limit_id"].as_str().unwrap_or("codex");
                normalize_rate_limit(limit, id)
            })
            .collect();
    }
    let mut snapshots = Vec::new();
    if let Some(primary) = value.get("rate_limit") {
        snapshots.push(normalize_rate_limit(primary, "codex"));
    }
    if let Some(additional) = value
        .get("additional_rate_limits")
        .and_then(Value::as_array)
    {
        snapshots.extend(additional.iter().map(|limit| {
            let id = limit["limit_name"].as_str().unwrap_or("codex");
            normalize_rate_limit(limit.get("rate_limit").unwrap_or(limit), id)
        }));
    }
    if snapshots.is_empty() && value.is_object() {
        let id = value["limit_id"].as_str().unwrap_or("codex");
        snapshots.push(normalize_rate_limit(value, id));
    }
    snapshots
}

fn normalize_rate_limit(value: &Value, id: &str) -> Value {
    json!({
        "limit_id": id,
        "limit_name": value.get("limit_name").cloned().unwrap_or(Value::Null),
        "primary": normalize_rate_limit_window(
            value.get("primary_window").or_else(|| value.get("primary"))
        ),
        "secondary": normalize_rate_limit_window(
            value.get("secondary_window").or_else(|| value.get("secondary"))
        ),
        "credits": value.get("credits").cloned().unwrap_or(Value::Null),
        "plan_type": value.get("plan_type").cloned().unwrap_or(Value::Null)
    })
}

fn normalize_rate_limit_window(window: Option<&Value>) -> Value {
    let Some(window) = window.filter(|window| !window.is_null()) else {
        return Value::Null;
    };
    let Some(used_percent) = window.get("used_percent").filter(|value| !value.is_null()) else {
        return Value::Null;
    };
    let window_minutes = window
        .get("window_minutes")
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| {
            let seconds = window.get("limit_window_seconds")?;
            if let Some(seconds) = seconds.as_i64() {
                Some(Value::from(seconds / 60))
            } else {
                seconds.as_f64().map(|seconds| Value::from(seconds / 60.0))
            }
        })
        .unwrap_or(Value::Null);
    let resets_at = window
        .get("resets_at")
        .or_else(|| window.get("reset_at"))
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "used_percent": used_percent,
        "window_minutes": window_minutes,
        "resets_at": resets_at
    })
}

fn rate_limit_headers(headers: &HeaderMap) -> Vec<Value> {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<f64>().ok())
    };
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let boolean = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<bool>().ok())
    };
    let mut prefixes = headers
        .keys()
        .filter_map(|name| {
            name.as_str()
                .strip_suffix("-primary-used-percent")
                .or_else(|| name.as_str().strip_suffix("-secondary-used-percent"))
        })
        .filter(|prefix| prefix.starts_with("x-codex"))
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    if headers.contains_key("x-codex-credits-balance") || headers.contains_key("x-codex-plan-type")
    {
        prefixes.insert("x-codex".to_owned());
    }
    let mut prefixes = prefixes.into_iter().collect::<Vec<_>>();
    prefixes.sort();
    prefixes
        .into_iter()
        .map(|prefix| {
            let limit_id = prefix
                .strip_prefix("x-codex-")
                .filter(|id| !id.is_empty())
                .unwrap_or("codex");
            let credits = (prefix == "x-codex").then(|| {
                json!({
                    "has_credits": boolean("x-codex-credits-has-credits"),
                    "unlimited": boolean("x-codex-credits-unlimited"),
                    "balance": number("x-codex-credits-balance")
                })
            });
            let window = |name: &str| {
                let used_percent = number(&format!("{prefix}-{name}-used-percent"));
                if used_percent.is_none() {
                    return Value::Null;
                }
                json!({
                    "used_percent": used_percent,
                    "window_minutes": number(&format!("{prefix}-{name}-window-minutes")),
                    "resets_at": number(&format!("{prefix}-{name}-reset-at"))
                })
            };
            json!({
                "limit_id": limit_id,
                "primary": window("primary"),
                "secondary": window("secondary"),
                "credits": credits,
                "plan_type": (prefix == "x-codex")
                    .then(|| text("x-codex-plan-type"))
                    .flatten()
            })
        })
        .collect()
}
