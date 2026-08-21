use std::{collections::HashSet, path::PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use atra_protocol::Model;
use reqwest::Client;
use serde::Deserialize;

use super::{
    ModelEventStream, ModelRequest, ProviderLoginStatus, ProviderRuntime, api,
    api_key_auth::ApiKeyAuth,
};

const PROVIDER_ID: &str = super::OPENCODE_GO_PROVIDER;
const API_BASE: &str = "https://opencode.ai/zen/go/v1";

pub(crate) struct OpenCodeGoProvider {
    client: Client,
    auth: ApiKeyAuth,
}

#[derive(Clone, Copy)]
enum Api {
    Responses,
    ChatCompletions,
    Messages,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelId>,
}

#[derive(Deserialize)]
struct ModelId {
    id: String,
}

impl OpenCodeGoProvider {
    pub(crate) fn new(auth_home: PathBuf) -> Self {
        Self {
            client: Client::new(),
            auth: ApiKeyAuth::new(auth_home, "OPENCODE_API_KEY"),
        }
    }

    async fn stream_inner(&self, request: &ModelRequest<'_>) -> Result<ModelEventStream> {
        let key = self.auth.key()?;
        let spec = spec(request.model)
            .with_context(|| format!("unsupported OpenCode Go model {}", request.model))?;
        match spec.api {
            Api::Responses => {
                api::responses::stream(
                    &self.client,
                    &format!("{API_BASE}/responses"),
                    &key,
                    request,
                    api::responses::Profile::Standard,
                )
                .await
            }
            Api::ChatCompletions => {
                api::chat_completions::stream(
                    &self.client,
                    &format!("{API_BASE}/chat/completions"),
                    &key,
                    request,
                )
                .await
            }
            Api::Messages => {
                api::messages::stream(&self.client, &format!("{API_BASE}/messages"), &key, request)
                    .await
            }
        }
    }
}

#[async_trait]
impl ProviderRuntime for OpenCodeGoProvider {
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
        let live = self
            .client
            .get(format!("{API_BASE}/models"))
            .send()
            .await
            .context("failed to load OpenCode Go models")?
            .error_for_status()
            .context("OpenCode Go model catalog failed")?
            .json::<ModelsResponse>()
            .await
            .context("failed to decode OpenCode Go model catalog")?
            .data
            .into_iter()
            .map(|model| model.id)
            .collect::<HashSet<_>>();
        for id in live.iter().filter(|id| spec(id).is_none()) {
            tracing::warn!(model = id, "hiding unknown OpenCode Go model");
        }
        Ok(SPECS
            .iter()
            .filter(|spec| live.contains(spec.id))
            .map(ModelSpec::model)
            .collect())
    }

    async fn login(&self, credential: Option<String>) -> Result<ProviderLoginStatus> {
        self.auth
            .login(credential.context("OpenCode Go login requires an API key")?)?;
        self.login_status().await
    }

    async fn login_status(&self) -> Result<ProviderLoginStatus> {
        Ok(if self.auth.configured() {
            ProviderLoginStatus::LoggedIn(Some(format!(
                "OpenCode Go API key ({})",
                self.auth
                    .source()
                    .map(|source| format!("{source:?}").to_lowercase())
                    .unwrap_or_else(|| "unknown".to_owned())
            )))
        } else {
            ProviderLoginStatus::LoginRequired
        })
    }

    async fn reload_auth(&self) -> Result<()> {
        self.auth.reload();
        Ok(())
    }

    async fn logout(&self) -> Result<()> {
        self.auth.logout()
    }

    async fn rate_limits(&self) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Array(Vec::new()))
    }

    async fn execute_tool(
        &self,
        model: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        let Some(binding) = spec(model).and_then(|spec| spec.binding(name)) else {
            return Ok(None);
        };
        super::tool_binding::execute(binding, &self.client, None, name, arguments).await
    }

    async fn stream(
        &self,
        _session_id: &str,
        request: &ModelRequest<'_>,
    ) -> Result<ModelEventStream> {
        self.stream_inner(request).await
    }

    fn context_tokens(&self, events: &[crate::storage::Event]) -> Result<usize> {
        super::api::ollama::context_tokens(events)
    }
}

struct ModelSpec {
    id: &'static str,
    name: &'static str,
    api: Api,
    context: i64,
    reasoning: &'static [&'static str],
    bindings: &'static [(&'static str, super::tool_binding::Binding)],
}

impl ModelSpec {
    fn model(&self) -> Model {
        let efforts = if self.reasoning.is_empty() {
            vec!["default".to_owned()]
        } else {
            self.reasoning
                .iter()
                .map(|value| (*value).to_owned())
                .collect()
        };
        Model {
            provider: PROVIDER_ID.to_owned(),
            id: self.id.to_owned(),
            display_name: self.name.to_owned(),
            description: Some(
                match self.api {
                    Api::Responses => "OpenCode Go · Responses",
                    Api::ChatCompletions => "OpenCode Go · Chat Completions",
                    Api::Messages => "OpenCode Go · Messages",
                }
                .to_owned(),
            ),
            default_reasoning_effort: efforts[0].clone(),
            supported_reasoning_efforts: efforts,
            context_window: Some(self.context),
            auto_compact_token_limit: Some(self.context * 9 / 10),
            tool_bindings: self
                .bindings
                .iter()
                .map(|(tool, binding)| atra_protocol::ModelToolBinding {
                    tool: (*tool).to_owned(),
                    implementation: binding.name().to_owned(),
                })
                .collect(),
        }
    }

    fn binding(&self, name: &str) -> Option<super::tool_binding::Binding> {
        self.bindings
            .iter()
            .find_map(|(tool, binding)| (*tool == name).then_some(*binding))
    }
}

fn spec(id: &str) -> Option<&'static ModelSpec> {
    SPECS.iter().find(|spec| spec.id == id)
}

const WEB_BINDINGS: &[(&str, super::tool_binding::Binding)] = &[
    (
        "web_search",
        super::tool_binding::Binding::Function(super::tool_binding::Executor::Exa),
    ),
    (
        "web_fetch",
        super::tool_binding::Binding::Function(super::tool_binding::Executor::DirectFetch),
    ),
];

static SPECS: &[ModelSpec] = &[
    ModelSpec {
        id: "grok-4.5",
        name: "Grok 4.5",
        api: Api::Responses,
        context: 500_000,
        reasoning: &["low", "medium", "high"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "gpt-5.6-luna",
        name: "GPT-5.6 Luna",
        api: Api::Responses,
        context: 1_050_000,
        reasoning: &["none", "low", "medium", "high", "xhigh", "max"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "glm-5.3",
        name: "GLM-5.3",
        api: Api::ChatCompletions,
        context: 1_000_000,
        reasoning: &["low", "high", "max"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "glm-5.2",
        name: "GLM-5.2",
        api: Api::ChatCompletions,
        context: 1_000_000,
        reasoning: &["high", "max"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "glm-5.1",
        name: "GLM-5.1",
        api: Api::ChatCompletions,
        context: 202_752,
        reasoning: &["default"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "kimi-k3",
        name: "Kimi K3",
        api: Api::ChatCompletions,
        context: 1_048_576,
        reasoning: &["max"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "kimi-k2.7-code",
        name: "Kimi K2.7 Code",
        api: Api::ChatCompletions,
        context: 262_144,
        reasoning: &["default"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "kimi-k2.6",
        name: "Kimi K2.6",
        api: Api::ChatCompletions,
        context: 262_144,
        reasoning: &["default"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
        api: Api::ChatCompletions,
        context: 1_000_000,
        reasoning: &["high", "max"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "deepseek-v4-flash",
        name: "DeepSeek V4 Flash",
        api: Api::ChatCompletions,
        context: 1_000_000,
        reasoning: &["low", "high", "max"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "mimo-v2.5",
        name: "MiMo V2.5",
        api: Api::ChatCompletions,
        context: 1_000_000,
        reasoning: &["default"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "mimo-v2.5-pro",
        name: "MiMo V2.5 Pro",
        api: Api::ChatCompletions,
        context: 1_048_576,
        reasoning: &["default"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "minimax-m3",
        name: "MiniMax-M3",
        api: Api::Messages,
        context: 1_000_000,
        reasoning: &["off", "on"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "minimax-m2.7",
        name: "MiniMax-M2.7",
        api: Api::Messages,
        context: 204_800,
        reasoning: &["default"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "muse-spark-1.2-contributor",
        name: "Muse Spark 1.2 Contributor",
        api: Api::Responses,
        context: 1_048_576,
        reasoning: &["minimal", "low", "medium", "high", "xhigh"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "qwen3.8-max",
        name: "Qwen3.8 Max",
        api: Api::Messages,
        context: 1_000_000,
        reasoning: &["off", "on"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "qwen3.7-max",
        name: "Qwen3.7 Max",
        api: Api::Messages,
        context: 1_000_000,
        reasoning: &["off", "on"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "qwen3.7-plus",
        name: "Qwen3.7 Plus",
        api: Api::Messages,
        context: 1_000_000,
        reasoning: &["off", "on"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "qwen3.6-plus",
        name: "Qwen3.6 Plus",
        api: Api::Messages,
        context: 1_000_000,
        reasoning: &["off", "on"],
        bindings: WEB_BINDINGS,
    },
    ModelSpec {
        id: "hy3",
        name: "Hy3",
        api: Api::ChatCompletions,
        context: 256_000,
        reasoning: &["none", "low", "high"],
        bindings: WEB_BINDINGS,
    },
];
