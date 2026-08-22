use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use atra_protocol::Model;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    ModelEventStream, ModelRequest, ProviderLoginStatus, ProviderRuntime, api_key_auth::ApiKeyAuth,
};

const PROVIDER_ID: &str = super::OLLAMA_PROVIDER;
const API_BASE: &str = "https://ollama.com/api";
pub(super) struct OllamaProvider {
    auth: ApiKeyAuth,
    client: reqwest::Client,
    models: tokio::sync::RwLock<Option<Vec<Model>>>,
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<Tag>,
}

#[derive(Deserialize)]
struct Tag {
    model: String,
    #[serde(default)]
    details: ModelDetails,
}

#[derive(Default, Deserialize)]
struct ModelDetails {
    #[serde(default)]
    parameter_size: String,
}

#[derive(Default, Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    model_info: serde_json::Map<String, Value>,
}

impl OllamaProvider {
    pub(super) fn new(auth_home: PathBuf) -> Self {
        Self {
            auth: ApiKeyAuth::new(auth_home, "OLLAMA_API_KEY"),
            client: reqwest::Client::new(),
            models: tokio::sync::RwLock::new(None),
        }
    }

    fn key(&self) -> Result<String> {
        self.auth.key()
    }

    fn authorized(&self, request: RequestBuilder) -> Result<RequestBuilder> {
        Ok(request.bearer_auth(self.key()?))
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response> {
        let response = self
            .authorized(request)?
            .send()
            .await
            .context("failed to call Ollama Cloud")?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            bail!("Ollama API key was rejected; run `atra provider login ollama`");
        }
        bail!("Ollama Cloud returned {status}: {body}")
    }

    async fn show(&self, model: &str) -> Result<ShowResponse> {
        self.send(
            self.client
                .post(format!("{API_BASE}/show"))
                .json(&json!({"model": model})),
        )
        .await?
        .json()
        .await
        .context("failed to decode Ollama model details")
    }

    pub(super) async fn chat(&self, body: Value) -> Result<Response> {
        self.send(self.client.post(format!("{API_BASE}/chat")).json(&body))
            .await
    }

    async fn catalog(&self) -> Result<Vec<Model>> {
        if let Some(models) = self.models.read().await.as_ref() {
            return Ok(models.clone());
        }
        let tags: TagsResponse = self
            .send(self.client.get(format!("{API_BASE}/tags")))
            .await?
            .json()
            .await
            .context("failed to decode Ollama model list")?;
        let mut models = Vec::with_capacity(tags.models.len());
        for tag in tags.models {
            let show = self.show(&tag.model).await?;
            let context_window = show
                .model_info
                .iter()
                .find(|(key, _)| key.ends_with(".context_length"))
                .and_then(|(_, value)| value.as_i64());
            let thinking = show.capabilities.iter().any(|value| value == "thinking");
            let (default_reasoning_effort, supported_reasoning_efforts) =
                super::api::ollama::reasoning_efforts(&tag.model, thinking);
            let description = (!tag.details.parameter_size.is_empty())
                .then(|| format!("{} parameters", tag.details.parameter_size));
            models.push(Model {
                provider: PROVIDER_ID.to_owned(),
                id: tag.model.clone(),
                display_name: tag.model,
                description,
                default_reasoning_effort,
                supported_reasoning_efforts,
                context_window,
                auto_compact_token_limit: context_window.map(|tokens| tokens * 4 / 5),
                tool_bindings: vec![
                    atra_protocol::ModelToolBinding {
                        tool: "web_search".to_owned(),
                        implementation: "ollama".to_owned(),
                    },
                    atra_protocol::ModelToolBinding {
                        tool: "web_fetch".to_owned(),
                        implementation: "ollama".to_owned(),
                    },
                ],
            });
        }
        *self.models.write().await = Some(models.clone());
        Ok(models)
    }
}

#[async_trait]
impl ProviderRuntime for OllamaProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn auth_method(&self) -> atra_protocol::ProviderAuthMethod {
        atra_protocol::ProviderAuthMethod::ApiKey
    }

    fn credential_source(&self) -> Option<atra_protocol::CredentialSource> {
        self.auth.source()
    }

    async fn models(&self) -> Result<Vec<Model>> {
        self.catalog().await
    }

    async fn login(&self, credential: Option<String>) -> Result<ProviderLoginStatus> {
        let credential = credential.context("Ollama login requires an API key")?;
        self.auth.login(credential)?;
        *self.models.write().await = None;
        self.catalog().await?;
        Ok(ProviderLoginStatus::LoggedIn(None))
    }

    async fn login_status(&self) -> Result<ProviderLoginStatus> {
        Ok(if self.auth.configured() {
            ProviderLoginStatus::LoggedIn(None)
        } else {
            ProviderLoginStatus::LoginRequired
        })
    }

    async fn reload_auth(&self) -> Result<()> {
        self.auth.reload();
        *self.models.write().await = None;
        Ok(())
    }

    async fn logout(&self) -> Result<()> {
        self.auth.logout()?;
        *self.models.write().await = None;
        Ok(())
    }

    async fn rate_limits(&self) -> Result<Value> {
        Ok(Value::Array(Vec::new()))
    }

    async fn execute_tool(
        &self,
        _model: &str,
        name: &str,
        arguments: &Value,
    ) -> Result<Option<Value>> {
        let executor = match name {
            "web_search" | "web_fetch" => super::tool_binding::Executor::Ollama,
            _ => return Ok(None),
        };
        let key = self.key()?;
        super::tool_binding::execute(
            super::tool_binding::Binding::Function(executor),
            &self.client,
            Some(&key),
            name,
            arguments,
        )
        .await
    }

    async fn stream(
        &self,
        session_id: &str,
        request: &ModelRequest<'_>,
    ) -> Result<ModelEventStream> {
        self.key()?;
        super::api::ollama::stream(self, session_id, request).await
    }
}
