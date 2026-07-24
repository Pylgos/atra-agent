use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use codex_api::{
    AuthProvider, Compression, ModelsClient, Reasoning, ResponseEvent, ResponsesApiRequest,
    ResponsesApiTools, ResponsesClient, ResponsesOptions,
};
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_login::{
    AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthManager, CodexAuth,
    default_client::create_client,
};
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::models::{
    ContentItem, FunctionCallOutputPayload, ResponseInputItem, ResponseItem,
};
use codex_protocol::openai_models::ModelVisibility;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::{json, value::RawValue};

use atra_protocol::Model;

use crate::storage::{Event, EventKind};

pub(crate) const DEFAULT_MODEL: &str = "gpt-5.6-sol";
const INSTRUCTIONS: &str = "You are Atra Agent. Use the provided tools when needed. \
Return a final answer after completing the user's request.";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelResponse {
    AssistantMessage {
        content: String,
    },
    ToolCall {
        name: String,
        arguments: serde_json::Value,
        #[serde(default)]
        call_id: Option<String>,
    },
}

pub(crate) enum Provider {
    Fake(FakeProvider),
    Codex(CodexProvider),
}

impl Provider {
    pub(crate) async fn models(&self) -> Result<Vec<Model>> {
        match self {
            Self::Fake(_) => Ok(vec![Model {
                id: DEFAULT_MODEL.to_owned(),
                display_name: DEFAULT_MODEL.to_owned(),
                description: None,
                default_reasoning_effort: "medium".to_owned(),
                supported_reasoning_efforts: ["low", "medium", "high", "xhigh"]
                    .map(str::to_owned)
                    .to_vec(),
            }]),
            Self::Codex(provider) => provider.models().await,
        }
    }

    pub(crate) async fn complete(
        &mut self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
    ) -> Result<ModelResponse> {
        match self {
            Self::Fake(provider) => provider.complete(events),
            Self::Codex(provider) => provider.complete(model, reasoning_effort, events).await,
        }
    }
}

pub(crate) struct FakeProvider {
    responses: VecDeque<ModelResponse>,
}

impl FakeProvider {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let script = fs::read(path)
            .with_context(|| format!("failed to read fake model script {}", path.display()))?;
        let responses = serde_json::from_slice(&script)
            .with_context(|| format!("failed to decode fake model script {}", path.display()))?;
        Ok(Self { responses })
    }

    fn complete(&mut self, events: &[Event]) -> Result<ModelResponse> {
        let mut response = self
            .responses
            .pop_front()
            .context("fake model script has no response remaining")?;
        if let ModelResponse::AssistantMessage { content } = &mut response
            && let Some(output) = events.iter().rev().find_map(|event| {
                (event.kind == EventKind::ToolResult)
                    .then(|| event.payload.pointer("/result/output")?.as_str())
                    .flatten()
            })
        {
            *content = content.replace("{{tool_output}}", output);
        }
        Ok(response)
    }
}

pub(crate) struct CodexProvider {
    auth: Arc<AuthManager>,
}

impl CodexProvider {
    pub(crate) async fn new(auth_home: PathBuf) -> Self {
        let route = codex_login::AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
            OutboundProxyPolicy::ReqwestDefault,
        ));
        Self {
            auth: Arc::new(
                AuthManager::new(
                    auth_home,
                    false,
                    AuthCredentialsStoreMode::File,
                    None,
                    None,
                    AuthKeyringBackendKind::default(),
                    route,
                )
                .await,
            ),
        }
    }

    pub(crate) async fn login_status(&self) -> Option<Option<String>> {
        self.auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .map(|auth| auth.get_account_email())
    }

    pub(crate) async fn reload_auth(&self) {
        self.auth.reload().await;
    }

    async fn models(&self) -> Result<Vec<Model>> {
        let auth = self
            .auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .context("Codex login required; run `atra codex login`")?;
        let provider = ModelProviderInfo::create_openai_provider(None)
            .to_api_provider(Some(auth.auth_mode()))
            .context("failed to configure Codex model endpoint")?;
        let request_url = ModelsClient::<codex_api::ReqwestTransport>::request_url(
            &provider,
            env!("CARGO_PKG_VERSION"),
        );
        let client = ModelsClient::new(
            codex_api::ReqwestTransport::from_http_client(create_client()),
            provider,
            Arc::new(BearerAuth::new(&auth)?),
        );
        let (models, _) = client
            .list_models(request_url, HeaderMap::new())
            .await
            .context("failed to list Codex models")?;
        let models = if models
            .iter()
            .any(|model| model.visibility == ModelVisibility::List)
        {
            models
        } else {
            let mut bundled = codex_models_manager::bundled_models_response()
                .context("failed to load bundled Codex model catalog")?
                .models;
            for model in models {
                if let Some(existing) = bundled
                    .iter_mut()
                    .find(|existing| existing.slug == model.slug)
                {
                    *existing = model;
                } else {
                    bundled.push(model);
                }
            }
            bundled
        };
        Ok(models
            .into_iter()
            .filter(|model| model.visibility == ModelVisibility::List)
            .map(|model| {
                let default_reasoning_effort = model
                    .default_reasoning_level
                    .unwrap_or_default()
                    .to_string();
                let mut supported_reasoning_efforts = model
                    .supported_reasoning_levels
                    .into_iter()
                    .map(|preset| preset.effort.to_string())
                    .collect::<Vec<_>>();
                if supported_reasoning_efforts.is_empty() {
                    supported_reasoning_efforts.push(default_reasoning_effort.clone());
                }
                Model {
                    id: model.slug,
                    display_name: model.display_name,
                    description: model.description,
                    default_reasoning_effort,
                    supported_reasoning_efforts,
                }
            })
            .collect())
    }

    async fn complete(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
    ) -> Result<ModelResponse> {
        let auth = self
            .auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .context("Codex login required; run `atra codex login`")?;
        let request = ResponsesApiRequest {
            model: model.to_owned(),
            instructions: INSTRUCTIONS.to_owned(),
            input: model_input(events)?,
            tools: Some(tool_definitions()?),
            tool_choice: "auto".to_owned(),
            parallel_tool_calls: false,
            reasoning: Some(Reasoning {
                effort: Some(
                    reasoning_effort
                        .parse()
                        .map_err(|error: String| anyhow::anyhow!(error))?,
                ),
                summary: None,
                context: None,
            }),
            store: false,
            stream: true,
            stream_options: None,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
        };
        let provider = ModelProviderInfo::create_openai_provider(None)
            .to_api_provider(Some(auth.auth_mode()))
            .context("failed to configure Codex model endpoint")?;
        let api_auth = Arc::new(BearerAuth::new(&auth)?);
        let client = ResponsesClient::new(
            codex_api::ReqwestTransport::from_http_client(create_client()),
            provider,
            api_auth,
        );
        let mut stream = client
            .stream_request(
                request,
                ResponsesOptions {
                    extra_headers: HeaderMap::new(),
                    compression: Compression::None,
                    ..Default::default()
                },
            )
            .await
            .context("Codex request failed")?;

        while let Some(event) = stream.next().await {
            if let ResponseEvent::OutputItemDone(item) =
                event.context("Codex response stream failed")?
                && let Some(response) = response_from_item(item)?
            {
                return Ok(response);
            }
        }
        bail!("Codex response ended without an assistant message or tool call")
    }
}

struct BearerAuth {
    token: HeaderValue,
    account_id: Option<HeaderValue>,
}

impl BearerAuth {
    fn new(auth: &CodexAuth) -> Result<Self> {
        Ok(Self {
            token: HeaderValue::from_str(&format!("Bearer {}", auth.get_token()?))
                .context("Codex access token is not a valid header value")?,
            account_id: auth
                .get_account_id()
                .map(|value| HeaderValue::from_str(&value))
                .transpose()
                .context("Codex account ID is not a valid header value")?,
        })
    }
}

impl AuthProvider for BearerAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        headers.insert(http::header::AUTHORIZATION, self.token.clone());
        if let Some(account_id) = &self.account_id {
            headers.insert("ChatGPT-Account-ID", account_id.clone());
        }
    }
}

fn model_input(events: &[Event]) -> Result<Vec<ResponseItem>> {
    events
        .iter()
        .filter_map(|event| {
            let item = match event.kind {
                EventKind::UserMessage => ResponseItem::from(ResponseInputItem::Message {
                    role: "user".to_owned(),
                    content: vec![ContentItem::InputText {
                        text: event.payload["content"].as_str()?.to_owned(),
                    }],
                    phase: None,
                }),
                EventKind::AssistantMessage => ResponseItem::from(ResponseInputItem::Message {
                    role: "assistant".to_owned(),
                    content: vec![ContentItem::OutputText {
                        text: event.payload["content"].as_str()?.to_owned(),
                    }],
                    phase: None,
                }),
                EventKind::ToolCall => ResponseItem::FunctionCall {
                    id: None,
                    name: event.payload["name"].as_str()?.to_owned(),
                    namespace: None,
                    arguments: event.payload["arguments"].to_string(),
                    call_id: event.payload["call_id"].as_str()?.to_owned(),
                    internal_chat_message_metadata_passthrough: None,
                },
                EventKind::ToolResult => {
                    ResponseItem::from(ResponseInputItem::FunctionCallOutput {
                        call_id: event.payload["call_id"].as_str()?.to_owned(),
                        output: FunctionCallOutputPayload::from_text(
                            serde_json::to_string(&event.payload["result"]).ok()?,
                        ),
                    })
                }
                EventKind::ApprovalRequest | EventKind::ApprovalResponse => return None,
            };
            Some(Ok(item))
        })
        .collect()
}

fn response_from_item(item: ResponseItem) -> Result<Option<ModelResponse>> {
    match item {
        ResponseItem::Message { content, .. } => {
            let content = content
                .into_iter()
                .filter_map(|item| match item {
                    ContentItem::OutputText { text } => Some(text),
                    ContentItem::InputText { .. }
                    | ContentItem::InputImage { .. }
                    | ContentItem::InputAudio { .. } => None,
                })
                .collect::<String>();
            Ok((!content.is_empty()).then_some(ModelResponse::AssistantMessage { content }))
        }
        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } => Ok(Some(ModelResponse::ToolCall {
            name,
            arguments: serde_json::from_str(&arguments)
                .context("Codex returned invalid tool arguments")?,
            call_id: Some(call_id),
        })),
        _ => Ok(None),
    }
}

fn tool_definitions() -> Result<ResponsesApiTools> {
    let tools = json!([
        {
            "type": "function",
            "name": "exec_command",
            "description": "Execute a Bash command on a named Atra Runner.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "runner": {"type": "string"},
                    "command": {"type": "string"},
                    "cwd": {"type": ["string", "null"]},
                    "background": {"type": "boolean"},
                    "timeout_ms": {"type": ["integer", "null"]},
                    "timeout_action": {
                        "type": "string",
                        "enum": ["return_running", "terminate"]
                    }
                },
                "required": ["runner", "command", "background", "timeout_action"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "apply_patch",
            "description": "Apply an Atra patch on a named Atra Runner.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "runner": {"type": "string"},
                    "patch": {"type": "string"},
                    "cwd": {"type": ["string", "null"]}
                },
                "required": ["runner", "patch"],
                "additionalProperties": false
            }
        }
    ]);
    let raw = RawValue::from_string(tools.to_string()).context("failed to encode tool schemas")?;
    Ok(Arc::<RawValue>::from(raw).into())
}
