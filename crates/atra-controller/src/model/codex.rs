use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use codex_api::{
    AuthProvider, CompactClient, CompactionInput, Compression, ModelsClient, Reasoning,
    ResponseEvent, ResponsesApiRequest, ResponsesApiTools, ResponsesClient, ResponsesOptions,
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
use serde_json::{json, value::RawValue};
use tokio::sync::{RwLock, mpsc};

use atra_protocol::Model;

use super::{ModelCompletion, ModelResponse};
use crate::storage::{Event, EventKind};

const INSTRUCTIONS: &str = "You are Atra Agent. Use the provided tools when needed. \
Return a final answer after completing the user's request.";

pub(crate) struct CodexProvider {
    auth: Arc<AuthManager>,
    models: RwLock<Option<Vec<Model>>>,
}

impl CodexProvider {
    pub(super) async fn new(auth_home: PathBuf) -> Self {
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
            models: RwLock::new(None),
        }
    }

    pub(super) async fn login_status(&self) -> Option<Option<String>> {
        self.auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .map(|auth| auth.get_account_email())
    }

    pub(super) async fn reload_auth(&self) {
        self.auth.reload().await;
        *self.models.write().await = None;
    }

    pub(super) async fn models(&self) -> Result<Vec<Model>> {
        if let Some(models) = self.models.read().await.as_ref() {
            return Ok(models.clone());
        }
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
        let models = models
            .into_iter()
            .filter(|model| model.visibility == ModelVisibility::List)
            .map(|model| {
                let context_window = model.context_window;
                let auto_compact_token_limit = model.auto_compact_token_limit();
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
                    context_window,
                    auto_compact_token_limit,
                }
            })
            .collect::<Vec<_>>();
        *self.models.write().await = Some(models.clone());
        Ok(models)
    }

    pub(super) async fn complete(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        deltas: Option<&mpsc::UnboundedSender<String>>,
        prompt_cache_key: &str,
    ) -> Result<ModelCompletion> {
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
            include: vec!["reasoning.encrypted_content".to_owned()],
            service_tier: None,
            prompt_cache_key: Some(prompt_cache_key.to_owned()),
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

        let mut response = None;
        let mut reasoning = Vec::new();
        let mut token_usage = None;
        while let Some(event) = stream.next().await {
            match event.context("Codex response stream failed")? {
                ResponseEvent::OutputTextDelta(delta) => {
                    if let Some(deltas) = deltas {
                        deltas.send(delta).ok();
                    }
                }
                ResponseEvent::OutputItemDone(item) => {
                    if matches!(item, ResponseItem::Reasoning { .. }) {
                        reasoning.push(item);
                    } else if let Some(item_response) = response_from_item(item)? {
                        response = Some(item_response);
                    }
                }
                ResponseEvent::Completed {
                    token_usage: usage, ..
                } => token_usage = usage,
                _ => {}
            }
        }
        Ok(ModelCompletion {
            response: response
                .context("Codex response ended without an assistant message or tool call")?,
            reasoning,
            token_usage,
        })
    }

    pub(super) async fn compact(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<Vec<ResponseItem>> {
        let auth = self
            .auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .context("Codex login required; run `atra codex login`")?;
        let provider = ModelProviderInfo::create_openai_provider(None)
            .to_api_provider(Some(auth.auth_mode()))
            .context("failed to configure Codex compaction endpoint")?;
        let client = CompactClient::new(
            codex_api::ReqwestTransport::from_http_client(create_client()),
            provider,
            Arc::new(BearerAuth::new(&auth)?),
        );
        let input = model_input(events)?;
        client
            .compact_input(
                &CompactionInput {
                    model,
                    input: &input,
                    instructions: INSTRUCTIONS,
                    tools: Some(tool_definitions()?),
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
                    service_tier: None,
                    prompt_cache_key: Some(prompt_cache_key),
                    text: None,
                },
                HeaderMap::new(),
                Duration::from_secs(300),
                None,
            )
            .await
            .context("Codex compaction failed")
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
    let mut input = Vec::new();
    let events = if let Some(index) = events
        .iter()
        .rposition(|event| event.kind == EventKind::Compaction)
    {
        input.extend(
            serde_json::from_value::<Vec<ResponseItem>>(events[index].payload["items"].clone())
                .context("stored compaction contains invalid response items")?,
        );
        &events[index + 1..]
    } else {
        events
    };
    input.extend(
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
                    EventKind::Reasoning => {
                        serde_json::from_value(event.payload["item"].clone()).ok()?
                    }
                    EventKind::ApprovalRequest
                    | EventKind::ApprovalResponse
                    | EventKind::Compaction
                    | EventKind::TokenUsage => return None,
                };
                Some(Ok(item))
            })
            .collect::<Result<Vec<_>>>()?,
    );
    Ok(input)
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
            "name": "list_runners",
            "description": "List the available Atra Runners and their roles. Use this when the appropriate execution environment is not already known.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }
        },
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
