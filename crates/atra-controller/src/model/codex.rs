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

use super::{ModelCompletion, ModelResponse, ModelStreamEvent};
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
            &codex_models_manager::client_version_to_whole(),
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
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
        prompt_cache_key: &str,
    ) -> Result<ModelCompletion> {
        let auth = self
            .auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .context("Codex login required; run `atra codex login`")?;
        let request = completion_request(model, reasoning_effort, events, prompt_cache_key)?;
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
                    if let Some(updates) = updates {
                        updates.send(ModelStreamEvent::AssistantDelta(delta)).ok();
                    }
                }
                ResponseEvent::OutputItemAdded(ResponseItem::CustomToolCall {
                    id: Some(item_id),
                    name,
                    ..
                }) => {
                    if let Some(updates) = updates {
                        updates
                            .send(ModelStreamEvent::ToolCallStarted {
                                item_id: item_id.to_string(),
                                name,
                            })
                            .ok();
                    }
                }
                ResponseEvent::ToolCallInputDelta { item_id, delta, .. } => {
                    if let Some(updates) = updates {
                        updates
                            .send(ModelStreamEvent::ToolCallDelta { item_id, delta })
                            .ok();
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

    pub(super) fn completion_snapshot(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<serde_json::Value> {
        serde_json::to_value(completion_request(
            model,
            reasoning_effort,
            events,
            prompt_cache_key,
        )?)
        .context("failed to encode Codex request snapshot")
    }

    pub(super) fn compaction_snapshot(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<serde_json::Value> {
        let input = model_input(events)?;
        serde_json::to_value(CompactionInput {
            model,
            input: &input,
            instructions: INSTRUCTIONS,
            tools: Some(tool_definitions()?),
            parallel_tool_calls: false,
            reasoning: Some(reasoning(reasoning_effort)?),
            service_tier: None,
            prompt_cache_key: Some(prompt_cache_key),
            text: None,
        })
        .context("failed to encode Codex compaction snapshot")
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
                    reasoning: Some(reasoning(reasoning_effort)?),
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

fn completion_request(
    model: &str,
    reasoning_effort: &str,
    events: &[Event],
    prompt_cache_key: &str,
) -> Result<ResponsesApiRequest> {
    Ok(ResponsesApiRequest {
        model: model.to_owned(),
        instructions: INSTRUCTIONS.to_owned(),
        input: model_input(events)?,
        tools: Some(tool_definitions()?),
        tool_choice: "auto".to_owned(),
        parallel_tool_calls: false,
        reasoning: Some(reasoning(reasoning_effort)?),
        store: false,
        stream: true,
        stream_options: None,
        include: vec!["reasoning.encrypted_content".to_owned()],
        service_tier: None,
        prompt_cache_key: Some(prompt_cache_key.to_owned()),
        text: None,
        client_metadata: None,
    })
}

fn reasoning(reasoning_effort: &str) -> Result<Reasoning> {
    Ok(Reasoning {
        effort: Some(
            reasoning_effort
                .parse()
                .map_err(|error: String| anyhow::anyhow!(error))?,
        ),
        summary: None,
        context: None,
    })
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
                    EventKind::ToolCall if event.payload["type"] == "custom" => {
                        ResponseItem::CustomToolCall {
                            id: None,
                            status: Some("completed".to_owned()),
                            call_id: event.payload["call_id"].as_str()?.to_owned(),
                            name: event.payload["name"].as_str()?.to_owned(),
                            namespace: None,
                            input: event.payload["input"].as_str()?.to_owned(),
                            internal_chat_message_metadata_passthrough: None,
                        }
                    }
                    EventKind::ToolCall => ResponseItem::FunctionCall {
                        id: None,
                        name: event.payload["name"].as_str()?.to_owned(),
                        namespace: None,
                        arguments: event.payload["arguments"].to_string(),
                        call_id: event.payload["call_id"].as_str()?.to_owned(),
                        internal_chat_message_metadata_passthrough: None,
                    },
                    EventKind::ToolResult if event.payload["type"] == "custom" => {
                        ResponseItem::from(ResponseInputItem::CustomToolCallOutput {
                            call_id: event.payload["call_id"].as_str()?.to_owned(),
                            name: event.payload["name"].as_str().map(str::to_owned),
                            output: FunctionCallOutputPayload::from_text(tool_result_text(
                                &event.payload["result"],
                            )),
                        })
                    }
                    EventKind::ToolResult => {
                        ResponseItem::from(ResponseInputItem::FunctionCallOutput {
                            call_id: event.payload["call_id"].as_str()?.to_owned(),
                            output: FunctionCallOutputPayload::from_text(tool_result_text(
                                &event.payload["result"],
                            )),
                        })
                    }
                    EventKind::Reasoning => {
                        serde_json::from_value(event.payload["item"].clone()).ok()?
                    }
                    EventKind::ApprovalRequest
                    | EventKind::ApprovalResponse
                    | EventKind::Compaction
                    | EventKind::ModelRequest
                    | EventKind::TokenUsage => return None,
                };
                Some(Ok(item))
            })
            .collect::<Result<Vec<_>>>()?,
    );
    Ok(input)
}

fn tool_result_text(result: &serde_json::Value) -> String {
    result
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| result.to_string())
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
        ResponseItem::CustomToolCall {
            id,
            name,
            input,
            call_id,
            ..
        } => Ok(Some(ModelResponse::CustomToolCall {
            item_id: id.map(String::from),
            name,
            input,
            call_id,
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
            "name": "wait_process",
            "description": "Wait for more output or completion from a background process on a named Atra Runner.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "runner": {"type": "string"},
                    "process_handle": {"type": "string"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["runner", "process_handle", "timeout_ms"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "write_process",
            "description": "Write text to the standard input of a background process on a named Atra Runner.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "runner": {"type": "string"},
                    "process_handle": {"type": "string"},
                    "input": {"type": "string"}
                },
                "required": ["runner", "process_handle", "input"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "stop_process",
            "description": "Stop a background process on a named Atra Runner.",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "runner": {"type": "string"},
                    "process_handle": {"type": "string"}
                },
                "required": ["runner", "process_handle"],
                "additionalProperties": false
            }
        },
        {
            "type": "custom",
            "name": "apply_patch",
            "description": "Apply an Atra patch. Put the target Runner name in the required `*** Environment ID: <runner>` line.",
            "format": {
                "type": "grammar",
                "syntax": "lark",
                "definition": "start: begin_patch environment_id hunk+ end_patch\nbegin_patch: \"*** Begin Patch\" LF\nenvironment_id: \"*** Environment ID: \" filename LF\nend_patch: \"*** End Patch\" LF?\n\nhunk: add_hunk | delete_hunk | update_hunk\nadd_hunk: \"*** Add File: \" filename LF add_line+\ndelete_hunk: \"*** Delete File: \" filename LF\nupdate_hunk: \"*** Update File: \" filename LF change_move? change?\n\nfilename: /(.+)/\nadd_line: \"+\" /(.*)/ LF -> line\n\nchange_move: \"*** Move to: \" filename LF\nchange: (change_context | change_line)+ eof_line?\nchange_context: (\"@@\" | \"@@ \" /(.+)/) LF\nchange_line: (\"+\" | \"-\" | \" \") /(.*)/ LF\neof_line: \"*** End of File\" LF\n\n%import common.LF"
            }
        }
    ]);
    let raw = RawValue::from_string(tools.to_string()).context("failed to encode tool schemas")?;
    Ok(Arc::<RawValue>::from(raw).into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn request_snapshot_contains_exact_context_and_omits_observer_events() {
        let events = vec![
            Event {
                sequence: 0,
                kind: EventKind::UserMessage,
                payload: json!({"content": "hello"}),
            },
            Event {
                sequence: 1,
                kind: EventKind::ModelRequest,
                payload: json!({"request": "observer-only"}),
            },
        ];

        let request = completion_request("model", "medium", &events, "cache").unwrap();
        let snapshot = serde_json::to_value(request).unwrap();

        assert_eq!(snapshot["model"], "model");
        assert_eq!(snapshot["prompt_cache_key"], "cache");
        assert_eq!(snapshot["input"].as_array().unwrap().len(), 1);
        assert_eq!(
            snapshot.pointer("/input/0/content/0/text"),
            Some(&json!("hello"))
        );
        assert!(
            snapshot["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "apply_patch")
        );
    }
}
