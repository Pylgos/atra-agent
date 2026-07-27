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
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_login::{
    AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthManager, CodexAuth,
    default_client::create_client,
};
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::{
    ContentItem, FunctionCallOutputPayload, ResponseInputItem, ResponseItem,
};
use codex_protocol::openai_models::ModelVisibility;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue};
use indoc::indoc;
use serde_json::{json, value::RawValue};
use tokio::sync::{Mutex, RwLock, mpsc};

use atra_protocol::{Model, Runner};

use super::{ModelCompletion, ModelResponse, ModelStreamEvent};
use crate::storage::{Event, EventKind};

const INSTRUCTIONS: &str = r#"You are Atra Agent. Fulfill the user's request using the provided tools when needed.

Commands, managed processes, and patches execute on Atra Runners. The available Runners are provided in the conversation context. For each tool call, choose a suitable Runner with no more access than the operation requires.

Do not bypass or weaken Runner restrictions, sandbox boundaries, or Controller approval decisions.

Use tool results to determine the next action, then return a final answer."#;
const SESSION_IDLE_TTL: Duration = Duration::from_secs(60 * 60);

pub(crate) struct CodexProvider {
    auth: Arc<AuthManager>,
    models: RwLock<Option<Vec<Model>>>,
    sessions: Mutex<HashMap<String, Arc<CodexSession>>>,
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
    }

    pub(super) async fn logout(&self) -> Result<()> {
        self.auth
            .logout_with_revoke()
            .await
            .context("failed to log out of Codex")?;
        *self.models.write().await = None;
        self.sessions.lock().await.clear();
        Ok(())
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
        read_completion(stream, updates).await
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
) -> Result<ModelCompletion> {
    let mut responses = Vec::new();
    let mut reasoning = Vec::new();
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
                if matches!(item, ResponseItem::Reasoning { .. }) {
                    reasoning.push(item);
                } else if let Some(item_response) = response_from_item(item)? {
                    responses.push(item_response);
                }
            }
            ResponseEvent::Completed {
                response_id: _,
                token_usage: usage,
                ..
            } => {
                token_usage = usage;
            }
            ResponseEvent::RateLimits(snapshot) => {
                rate_limits.push(snapshot);
            }
            _ => {}
        }
    }

    if responses.is_empty() {
        anyhow::bail!("Codex response ended without an assistant message or tool call");
    }
    Ok(ModelCompletion {
        responses,
        reasoning,
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
        .rposition(|event| event.kind == EventKind::Compaction)
    {
        items.extend(
            serde_json::from_value::<Vec<ResponseItem>>(events[index].payload["items"].clone())
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
        let Some(item) = (|| {
            Some(match event.kind {
                EventKind::WorkspaceInstructions => {
                    let transition = event.payload["transition"].as_str()?;
                    let text = match transition {
                        "initial" => event.payload["content"].as_str()?.to_owned(),
                        "replacement" => format!(
                            "These AGENTS.md instructions replace all previously provided \
                                 AGENTS.md instructions.\n\n{}",
                            event.payload["content"].as_str()?
                        ),
                        "removal" => {
                            "The previously provided AGENTS.md instructions no longer apply."
                                .to_owned()
                        }
                        _ => return None,
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
                EventKind::Skills => {
                    let transition = event.payload["transition"].as_str()?;
                    let text = match transition {
                        "initial" => event.payload["content"].as_str()?.to_owned(),
                        "replacement" => format!(
                            "This skills list replaces all previously provided skills.\n\n{}",
                            event.payload["content"].as_str()?
                        ),
                        "removal" => {
                            "The previously provided skills are no longer available.".to_owned()
                        }
                        _ => return None,
                    };
                    ResponseItem::from(ResponseInputItem::Message {
                        role: "developer".to_owned(),
                        content: vec![ContentItem::InputText { text }],
                        phase: None,
                    })
                }
                EventKind::Runners => {
                    let transition = event.payload["transition"].as_str()?;
                    let runners =
                        serde_json::from_value::<Vec<Runner>>(event.payload["runners"].clone())
                            .ok()?;
                    let list = format_runners(&runners);
                    let text = match transition {
                        "initial" => list,
                        "replacement" => format!(
                            "The available Atra Runner list has changed. This list replaces \
                                 the previously provided list.\n\n{list}"
                        ),
                        _ => return None,
                    };
                    ResponseItem::from(ResponseInputItem::Message {
                        role: "developer".to_owned(),
                        content: vec![ContentItem::InputText { text }],
                        phase: None,
                    })
                }
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
                EventKind::WebSearch => {
                    serde_json::from_value(event.payload["item"].clone()).ok()?
                }
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
                            projected_tool_result(event, &masked_sequences),
                        )),
                    })
                }
                EventKind::ToolResult => {
                    ResponseItem::from(ResponseInputItem::FunctionCallOutput {
                        call_id: event.payload["call_id"].as_str()?.to_owned(),
                        output: FunctionCallOutputPayload::from_text(tool_result_text(
                            projected_tool_result(event, &masked_sequences),
                        )),
                    })
                }
                EventKind::Reasoning => {
                    serde_json::from_value(event.payload["item"].clone()).ok()?
                }
                EventKind::Compaction
                | EventKind::FrozenBoundary
                | EventKind::ModelRequest
                | EventKind::TokenUsage
                | EventKind::RateLimits => return None,
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
    masked_sequences: &HashSet<i64>,
) -> &'a serde_json::Value {
    if masked_sequences.contains(&event.sequence) {
        &event.payload["masked_result"]
    } else {
        &event.payload["result"]
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
        item @ ResponseItem::WebSearchCall { .. } => Ok(Some(ModelResponse::WebSearch { item })),
        _ => Ok(None),
    }
}

fn tool_definitions() -> Result<ResponsesApiTools> {
    let tools = json!([
        {
            "type": "web_search",
            "external_web_access": true
        },
        {
            "type": "custom",
            "name": "runner",
            "description": indoc! {"
                Execute one or more operations on named Atra Runners.
                Start each group with `*** Runner <runner>`; repeat it to switch Runners.

                Processes:
                Use `*** Command` to wait up to 10000 milliseconds for a Bash command and leave it running if unfinished.
                Use `*** Timed Command <milliseconds>` to stop a command if it exceeds the specified duration.
                Use `*** Background Command <process-id>` to start a named managed process without waiting for it to finish.
                End every command with `*** End`.
                Use `*** Wait <process-id> <milliseconds>` to wait for more output or completion.
                Use `*** Stop <process-id>` to stop a managed process.
                Process IDs are local to each Runner within the current conversation and must match `[a-z][a-z0-9_-]{0,63}`.

                Patches:
                Use `*** Patch` and `*** End` to add, update, delete, or move files.
                Paths in patches are relative to the current Runner's working directory unless absolute.
                Use line ranges for large deletions or replacements when the line numbers are already known.
                When inspecting a file is otherwise necessary, obtain line numbers as part of that inspection.
                Use ordinary diff lines for small changes.
                Do not make an additional operation solely to obtain line numbers unless doing so avoids a substantially larger patch.

                Operations execute sequentially, and their results are returned together after all operations have finished.
                Use a separate tool call when a result is needed to decide the next operation.
            "},
            "format": {
                "type": "grammar",
                "syntax": "lark",
                "definition": indoc! {r#"
                    start: runner_group+
                    runner_group: runner operation+
                    runner: "*** Runner " name LF
                    operation: command | patch | wait_process | stop_process

                    command: foreground_command | background_command | timed_command
                    foreground_command: "*** Command" LF command_body
                    background_command: "*** Background Command " process_id LF command_body
                    timed_command: "*** Timed Command " INT LF command_body
                    command_body: command_line+ END
                    command_line: /(.+)/ LF | LF
                    END: /\*\*\* End\r?\n/

                    patch: "*** Patch" LF hunk+ END
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

                    wait_process: "*** Wait " process_id " " INT LF
                    stop_process: "*** Stop " process_id LF
                    process_id: /[a-z][a-z0-9_-]{0,63}/

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
                .any(|tool| tool["name"] == "runner")
        );
    }
}
