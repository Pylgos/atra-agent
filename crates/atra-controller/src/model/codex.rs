use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use codex_api::{
    AuthProvider, CompactClient, CompactionInput, ModelsClient, Reasoning, ResponseEvent,
    ResponseStream, ResponsesApiRequest, ResponsesApiTools, ResponsesClient,
};
use codex_backend_client::Client as BackendClient;
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_login::{
    AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthManager, CodexAuth,
    default_client::create_client,
};
use codex_model_provider_info::{CHATGPT_CODEX_BASE_URL, ModelProviderInfo};
use codex_protocol::models::{
    ContentItem, FunctionCallOutputPayload, ResponseInputItem, ResponseItem,
};
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::{config_types::ReasoningSummary, protocol::RateLimitSnapshot};
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue};
use indoc::indoc;
use serde_json::{json, value::RawValue};
use tokio::sync::{Mutex, RwLock, mpsc};

use atra_protocol::{Model, Runner};

use super::{ModelCompletion, ModelStreamEvent, response_from_item};
use crate::storage::Event;
use atra_protocol::{InstructionEvent, RunnersEvent, ThreadEventData, ToolResultEvent};

const INSTRUCTIONS: &str = indoc! {r#"
    You are an expert coding assistant operating inside Atra, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files

    Commands execute on Atra Runners. The available Runners are provided in the conversation context. For each tool call, choose a suitable Runner with no more access than the operation requires.

    Guidelines:
    - Be concise in your responses.
    - Show file paths clearly when working with files.
    - Use update_todos to track non-trivial multi-step work. Keep at most one todo in progress, and mark every todo completed before ending the task.
    - Do not bypass or weaken Runner restrictions, sandbox boundaries, or Controller approval decisions."#};
const SESSION_IDLE_TTL: Duration = Duration::from_secs(60 * 60);

pub(crate) struct CodexProvider {
    auth: Arc<AuthManager>,
    models: RwLock<Option<Vec<Model>>>,
    sessions: Mutex<HashMap<String, Arc<CodexSession>>>,
    rate_limits: Arc<RwLock<Vec<RateLimitSnapshot>>>,
}

struct CodexSession {
    responses: ResponsesClient<codex_api::ReqwestTransport>,
    compact: CompactClient<codex_api::ReqwestTransport>,
    session_id: String,
    last_used_at: StdMutex<Instant>,
}

impl CodexSession {
    fn touch(&self) {
        *self
            .last_used_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }

    fn is_idle(&self, now: Instant) -> bool {
        now.duration_since(
            *self
                .last_used_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ) >= SESSION_IDLE_TTL
    }
}

pub(crate) struct CodexTurn {
    session: Arc<CodexSession>,
    turn_state: Arc<OnceLock<String>>,
    rate_limits: Arc<RwLock<Vec<RateLimitSnapshot>>>,
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
            sessions: Mutex::new(HashMap::new()),
            rate_limits: Arc::new(RwLock::new(Vec::new())),
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
        self.sessions.lock().await.clear();
        self.rate_limits.write().await.clear();
    }

    pub(super) async fn logout(&self) -> Result<()> {
        self.auth
            .logout_with_revoke()
            .await
            .context("failed to log out of Codex")?;
        *self.models.write().await = None;
        self.sessions.lock().await.clear();
        self.rate_limits.write().await.clear();
        Ok(())
    }

    pub(super) async fn rate_limits(&self) -> Result<Vec<RateLimitSnapshot>> {
        let auth = self
            .auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .context("Codex login required; run `atra codex login`")?;
        let base_url = CHATGPT_CODEX_BASE_URL
            .strip_suffix("/codex")
            .expect("Codex base URL ends in /codex");
        let snapshots = BackendClient::from_auth(
            base_url,
            &auth,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .get_rate_limits_many()
        .await
        .context("failed to fetch Codex rate limits")?;
        *self.rate_limits.write().await = snapshots.clone();
        Ok(snapshots)
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

    pub(super) async fn start_turn(&self, session_id: String) -> Result<CodexTurn> {
        let now = Instant::now();
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, session| !session.is_idle(now));
        if let Some(session) = sessions.get(&session_id) {
            session.touch();
            return Ok(CodexTurn {
                session: session.clone(),
                turn_state: Arc::new(OnceLock::new()),
                rate_limits: self.rate_limits.clone(),
            });
        }
        drop(sessions);
        let auth = self
            .auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .context("Codex login required; run `atra codex login`")?;
        let provider = ModelProviderInfo::create_openai_provider(None)
            .to_api_provider(Some(auth.auth_mode()))
            .context("failed to configure Codex model endpoint")?;
        let api_auth = Arc::new(BearerAuth::new(&auth)?);
        let session = Arc::new(CodexSession {
            responses: ResponsesClient::new(
                codex_api::ReqwestTransport::from_http_client(create_client()),
                provider.clone(),
                api_auth.clone(),
            ),
            compact: CompactClient::new(
                codex_api::ReqwestTransport::from_http_client(create_client()),
                provider,
                api_auth,
            ),
            session_id,
            last_used_at: StdMutex::new(now),
        });
        let mut sessions = self.sessions.lock().await;
        let session = if let Some(cached) = sessions.get(&session.session_id) {
            cached.touch();
            cached.clone()
        } else {
            sessions.insert(session.session_id.clone(), session.clone());
            session
        };
        Ok(CodexTurn {
            session,
            turn_state: Arc::new(OnceLock::new()),
            rate_limits: self.rate_limits.clone(),
        })
    }

    pub(super) fn completion_snapshot(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<serde_json::Value> {
        completion_request(model, reasoning_effort, events, prompt_cache_key)
    }

    pub(super) fn compaction_snapshot(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<serde_json::Value> {
        compaction_request(model, reasoning_effort, events, prompt_cache_key)
    }
}

impl CodexTurn {
    pub(super) async fn complete(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
        prompt_cache_key: &str,
    ) -> Result<ModelCompletion> {
        self.session.touch();
        let request = completion_request(model, reasoning_effort, events, prompt_cache_key)?;
        let mut headers = HeaderMap::new();
        let session_id = HeaderValue::from_str(&self.session.session_id)
            .context("model session ID is not a valid header value")?;
        headers.insert("session-id", session_id.clone());
        headers.insert("thread-id", session_id.clone());
        headers.insert("x-client-request-id", session_id);
        let stream = self
            .session
            .responses
            .stream(
                request,
                headers,
                codex_api::Compression::None,
                Some(self.turn_state.clone()),
            )
            .await
            .context("Codex request failed")?;
        read_completion(stream, updates, &self.rate_limits).await
    }

    pub(super) async fn compact(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<Vec<ResponseItem>> {
        self.session.touch();
        let request = compaction_request(model, reasoning_effort, events, prompt_cache_key)?;
        let mut headers = HeaderMap::new();
        let session_id = HeaderValue::from_str(&self.session.session_id)
            .context("model session ID is not a valid header value")?;
        headers.insert("session-id", session_id.clone());
        headers.insert("thread-id", session_id.clone());
        headers.insert("x-client-request-id", session_id);
        self.session
            .compact
            .compact(
                request,
                headers,
                Duration::from_secs(300),
                Some(self.turn_state.as_ref()),
            )
            .await
            .context("Codex compaction failed")
    }
}

async fn read_completion(
    mut stream: ResponseStream,
    updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    latest_rate_limits: &RwLock<Vec<RateLimitSnapshot>>,
) -> Result<ModelCompletion> {
    let mut output = Vec::new();
    let mut response_id = None;
    let mut token_usage = None;
    let mut rate_limits = Vec::new();

    while let Some(event) = stream.next().await {
        let event = event.context("Codex response stream failed")?;
        match event {
            ResponseEvent::OutputTextDelta(delta) => {
                if let Some(updates) = updates {
                    let _ = updates.send(ModelStreamEvent::AssistantDelta(delta));
                }
            }
            ResponseEvent::ReasoningSummaryDelta { delta, .. } => {
                if let Some(updates) = updates {
                    let _ = updates.send(ModelStreamEvent::ReasoningSummaryDelta(delta));
                }
            }
            ResponseEvent::ReasoningSummaryPartAdded { .. } => {
                if let Some(updates) = updates {
                    let _ = updates.send(ModelStreamEvent::ReasoningSummaryPartAdded);
                }
            }
            ResponseEvent::OutputItemAdded(ResponseItem::CustomToolCall {
                id: Some(item_id),
                name,
                ..
            }) => {
                if let Some(updates) = updates {
                    let _ = updates.send(ModelStreamEvent::ToolCallStarted {
                        item_id: item_id.to_string(),
                        name,
                    });
                }
            }
            ResponseEvent::ToolCallInputDelta { item_id, delta, .. } => {
                if let Some(updates) = updates {
                    let _ = updates.send(ModelStreamEvent::ToolCallDelta { item_id, delta });
                }
            }
            ResponseEvent::OutputItemDone(item) => {
                output.push(item);
            }
            ResponseEvent::Completed {
                response_id: id,
                token_usage: usage,
                ..
            } => {
                response_id = Some(id);
                token_usage = usage;
            }
            ResponseEvent::RateLimits(snapshot) => {
                let mut latest = latest_rate_limits.write().await;
                let limit_id = snapshot.limit_id.as_deref().unwrap_or("codex");
                if let Some(current) = latest
                    .iter_mut()
                    .find(|current| current.limit_id.as_deref().unwrap_or("codex") == limit_id)
                {
                    *current = snapshot.clone();
                } else {
                    latest.push(snapshot.clone());
                }
                rate_limits.push(snapshot);
            }
            _ => {}
        }
    }

    if !output
        .iter()
        .cloned()
        .map(response_from_item)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .any(|response| !matches!(response, super::ModelResponse::Reasoning { .. }))
    {
        anyhow::bail!("Codex response ended without an assistant message or tool call");
    }
    Ok(ModelCompletion {
        output,
        response_id,
        token_usage,
        rate_limits,
    })
}

fn completion_request(
    model: &str,
    reasoning_effort: &str,
    events: &[Event],
    prompt_cache_key: &str,
) -> Result<serde_json::Value> {
    serde_json::to_value(ResponsesApiRequest {
        model: model.to_owned(),
        instructions: INSTRUCTIONS.to_owned(),
        input: model_input(events)?,
        tools: Some(tool_definitions()?),
        tool_choice: "auto".to_owned(),
        parallel_tool_calls: true,
        reasoning: Some(reasoning(reasoning_effort)?),
        store: false,
        stream: true,
        stream_options: None,
        include: vec!["reasoning.encrypted_content".to_owned()],
        service_tier: None,
        prompt_cache_key: Some(prompt_cache_key.to_owned()),
        text: None,
        client_metadata: Some(HashMap::from([
            ("session_id".to_owned(), prompt_cache_key.to_owned()),
            ("thread_id".to_owned(), prompt_cache_key.to_owned()),
        ])),
    })
    .context("failed to encode Codex request")
}

fn compaction_request(
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
    .context("failed to encode Codex compaction request")
}

fn reasoning(reasoning_effort: &str) -> Result<Reasoning> {
    Ok(Reasoning {
        effort: Some(
            reasoning_effort
                .parse()
                .map_err(|error: String| anyhow::anyhow!(error))?,
        ),
        summary: Some(ReasoningSummary::Detailed),
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
    let mut items = Vec::new();
    let events = if let Some(index) = events
        .iter()
        .rposition(|event| matches!(event.data, ThreadEventData::Compaction(_)))
    {
        items.extend(
            serde_json::from_value::<Vec<ResponseItem>>(match &events[index].data {
                ThreadEventData::Compaction(compaction) => compaction.items.clone(),
                _ => unreachable!(),
            })
            .context("stored compaction contains invalid response items")?,
        );
        &events[index + 1..]
    } else {
        events
    };
    let masked_sequences = crate::storage::latest_frozen_boundary(events)
        .map(|boundary| {
            boundary
                .masked_sequences
                .into_iter()
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    for event in events {
        if let ThreadEventData::ModelOutput(event) = &event.data {
            items.extend(
                serde_json::from_value::<Vec<ResponseItem>>(event.output.clone())
                    .context("stored model output contains invalid response items")?,
            );
            continue;
        }
        let Some(item) = (|| {
            Some(match &event.data {
                ThreadEventData::WorkspaceInstructions(instructions) => {
                    let text = match instructions {
                        InstructionEvent::Initial(content) => content.clone(),
                        InstructionEvent::Replacement(content) => format!(
                            "These AGENTS.md instructions replace all previously provided \
                                 AGENTS.md instructions.\n\n{}",
                            content
                        ),
                        InstructionEvent::Removal => {
                            "The previously provided AGENTS.md instructions no longer apply."
                                .to_owned()
                        }
                    };
                    ResponseItem::from(ResponseInputItem::Message {
                        role: "developer".to_owned(),
                        content: vec![ContentItem::InputText {
                            text: format!(
                                "# AGENTS.md instructions\n\n<INSTRUCTIONS>\n{text}\n</INSTRUCTIONS>"
                            ),
                        }],
                        phase: None,
                    })
                }
                ThreadEventData::Skills(instructions) => {
                    let text = match instructions {
                        InstructionEvent::Initial(content) => content.clone(),
                        InstructionEvent::Replacement(content) => format!(
                            "This skills list replaces all previously provided skills.\n\n{}",
                            content
                        ),
                        InstructionEvent::Removal => {
                            "The previously provided skills are no longer available.".to_owned()
                        }
                    };
                    ResponseItem::from(ResponseInputItem::Message {
                        role: "developer".to_owned(),
                        content: vec![ContentItem::InputText { text }],
                        phase: None,
                    })
                }
                ThreadEventData::Runners(event) => {
                    let text = match event {
                        RunnersEvent::Initial(runners) => format_runners(runners),
                        RunnersEvent::Replacement(runners) => format!(
                            "The available Atra Runner list has changed. This list replaces \
                                 the previously provided list.\n\n{}",
                            format_runners(runners)
                        ),
                    };
                    ResponseItem::from(ResponseInputItem::Message {
                        role: "developer".to_owned(),
                        content: vec![ContentItem::InputText { text }],
                        phase: None,
                    })
                }
                ThreadEventData::UserMessage(message) => {
                    ResponseItem::from(ResponseInputItem::Message {
                        role: "user".to_owned(),
                        content: vec![ContentItem::InputText {
                            text: message.content.clone(),
                        }],
                        phase: None,
                    })
                }
                ThreadEventData::ToolResult(ToolResultEvent::Custom { call_id, name, .. }) => {
                    ResponseItem::from(ResponseInputItem::CustomToolCallOutput {
                        call_id: call_id.clone()?,
                        name: Some(name.clone()),
                        output: FunctionCallOutputPayload::from_text(tool_result_text(
                            projected_tool_result(event, &masked_sequences),
                        )),
                    })
                }
                ThreadEventData::ToolResult(ToolResultEvent::Function { call_id, .. }) => {
                    ResponseItem::from(ResponseInputItem::FunctionCallOutput {
                        call_id: call_id.clone()?,
                        output: FunctionCallOutputPayload::from_text(tool_result_text(
                            projected_tool_result(event, &masked_sequences),
                        )),
                    })
                }
                ThreadEventData::AssistantMessage(_)
                | ThreadEventData::WebSearch(_)
                | ThreadEventData::ToolCall(_)
                | ThreadEventData::Reasoning(_)
                | ThreadEventData::ModelOutput(_)
                | ThreadEventData::Compaction(_)
                | ThreadEventData::FrozenBoundary(_)
                | ThreadEventData::ModelRequest(_)
                | ThreadEventData::TokenUsage(_)
                | ThreadEventData::RateLimits(_) => return None,
            })
        })() else {
            continue;
        };
        items.push(item);
    }
    Ok(items)
}

fn projected_tool_result<'a>(
    event: &'a Event,
    masked_sequences: &HashSet<atra_protocol::EventSequence>,
) -> &'a serde_json::Value {
    let (result, masked_result) = match &event.data {
        ThreadEventData::ToolResult(ToolResultEvent::Custom {
            result,
            masked_result,
            ..
        })
        | ThreadEventData::ToolResult(ToolResultEvent::Function {
            result,
            masked_result,
            ..
        }) => (result, masked_result),
        _ => unreachable!("projected tool result called with another event"),
    };
    if masked_sequences.contains(&event.sequence) {
        masked_result.as_ref().unwrap_or(result)
    } else {
        result
    }
}

pub(super) fn context_tokens(events: &[Event]) -> Result<usize> {
    let input = serde_json::to_string(&model_input(events)?)
        .context("failed to encode model input for token counting")?;
    Ok(super::text_tokens(&input))
}

fn tool_result_text(result: &serde_json::Value) -> String {
    result
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| result.to_string())
}

fn format_runners(runners: &[Runner]) -> String {
    if runners.is_empty() {
        return "No Atra Runners are currently available.".to_owned();
    }

    let mut lines = vec!["Available Atra Runners:".to_owned()];
    for runner in runners {
        lines.push(format!(
            "{}: {}",
            runner.name,
            runner
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        ));
    }
    lines.join("\n")
}

fn tool_definitions() -> Result<ResponsesApiTools> {
    let tools = json!([
        {
            "type": "web_search",
            "external_web_access": true
        },
        {
            "type": "function",
            "name": "update_todos",
            "description": indoc! {"
                Updates the task todo list.
                Provide an optional explanation and a list of todos, each with a step and status.
                At most one todo can be in_progress at a time.
            "},
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "explanation": {
                        "type": "string",
                        "description": "Optional explanation for this todo update."
                    },
                    "todos": {
                        "type": "array",
                        "description": "The list of todos.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {
                                    "type": "string",
                                    "description": "Todo step text."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Todo status."
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }
        },
        {
            "type": "custom",
            "name": "command",
            "description": indoc! {"
                Execute one or more Bash commands on named Atra Runners.
                Start each command with `*** Runner <runner>`; repeat it to run another command or switch Runners.
                A command ends at the next `*** Runner <runner>` line or the end of the tool input.

                Processes:
                Each command waits up to 120000 milliseconds and is left running if unfinished.
                Process IDs are local to each Runner within the current conversation and must match `[a-z][a-z0-9_-]{0,63}`.
                Run `atri proc spawn <process-id> '<command>'` to start a named managed process without waiting.
                Run `atri proc wait <process-id>... [--timeout <seconds>]` to wait for all named processes. The timeout defaults to 10 seconds and may not exceed 60 seconds.
                Run `atri proc stop <process-id>...` to stop named processes.
                These commands report every process in argument order. A wait timeout reports processes as running and does not fail.

                Patches:
                Run `atri patch` as a foreground command and pass the patch on standard input to add, update, delete, or move files.
                Use a quoted Bash heredoc so the patch is passed literally.
                Patch hunks start with `*** Add File: <path>`, `*** Update File: <path>`, or `*** Delete File: <path>`; a move follows an update header with `*** Move to: <path>`.
                Enclose the hunks with `*** Begin Patch` and `*** End Patch` on their own lines.
                Paths in patches are relative to the command's working directory unless absolute.
                Use line ranges for large deletions or replacements when the line numbers are already known.
                When inspecting a file is otherwise necessary, obtain line numbers as part of that inspection.
                Use ordinary diff lines for small changes.
                Do not make an additional operation solely to obtain line numbers unless doing so avoids a substantially larger patch.

                Commands in one tool call execute sequentially, and their results are returned together after all commands have finished.
                Use a separate tool call when a result is needed to decide the next operation.
            "},
            "format": {
                "type": "grammar",
                "syntax": "lark",
                "definition": indoc! {r#"
                    start: runner_group+
                    runner_group: runner command_item+
                    runner: "*** Runner " name LF

                    ?command_item: command_line | patch
                    command_line: /([^*].*|\*[^*].*|\*\*[^*].*|\*\*\*[^ ].*|\*|\*\*|\*\*\*)/ LF? | LF

                    patch: PATCH_BEGIN LF hunk+ PATCH_END LF
                    PATCH_BEGIN: "*** Begin Patch"
                    PATCH_END: "*** End Patch"
                    hunk: add_hunk | delete_hunk | update_hunk
                    add_hunk: "*** Add File: " filename LF add_line+
                    delete_hunk: "*** Delete File: " filename LF
                    update_hunk: "*** Update File: " filename LF change_move? first_update following_update*

                    name: /(.+)/
                    filename: /(.+)/
                    add_line: "+" /(.*)/ LF -> line

                    change_move: "*** Move to: " filename LF
                    first_update: change | range_change
                    following_update: headed_change | range_change
                    change: change_context? change_line+ eof_line?
                    headed_change: change_context change_line+ eof_line?
                    change_context: ("@@" | "@@ " /(.+)/) LF
                    change_line: ("+" | "-" | " ") /(.*)/ LF
                    eof_line: "*** End of File" LF

                    range_change: range_start remove_line (range_end remove_line)? add_line*
                    range_start: "@ start " INT LF
                    range_end: "@ end " INT LF
                    remove_line: "-" /(.*)/ LF

                    %import common.INT
                    %import common.LF
                "#}
            }
        },
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
                sequence: atra_protocol::EventSequence(0),
                data: ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                    content: "hello".to_owned(),
                }),
            },
            Event {
                sequence: atra_protocol::EventSequence(1),
                data: ThreadEventData::ModelRequest(atra_protocol::ModelRequestEvent {
                    kind: atra_protocol::ModelRequestKind::Response,
                    started_at_ms: 0,
                    request: json!("observer-only"),
                    context_window: None,
                    auto_compact_token_limit: None,
                    compacted: false,
                }),
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
                .any(|tool| tool["name"] == "command")
        );
    }
}
