use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Error, Result};
use codex_api::{
    ApiError, AuthProvider, CompactClient, CompactionInput, Compression, ModelsClient, Reasoning,
    ResponseCreateWsRequest, ResponseEvent, ResponseStream, ResponsesApiRequest, ResponsesApiTools,
    ResponsesClient, ResponsesOptions, ResponsesWebsocketClient, ResponsesWebsocketConnection,
    ResponsesWsRequest, TransportError,
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
use serde_json::{json, value::RawValue};
use tokio::sync::{Mutex, RwLock, mpsc};

use atra_protocol::Model;

use super::{ModelCompletion, ModelResponse, ModelStreamEvent};
use crate::storage::{Event, EventKind};

const INSTRUCTIONS: &str = "You are Atra Agent. Use the provided tools when needed. \
Return a final answer after completing the user's request.";
const WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";
const SESSION_IDLE_TTL: Duration = Duration::from_secs(60 * 60);

pub(crate) struct CodexProvider {
    auth: Arc<AuthManager>,
    models: RwLock<Option<Vec<Model>>>,
    sessions: Mutex<HashMap<String, Arc<CodexSession>>>,
}

struct CodexSession {
    responses: ResponsesClient<codex_api::ReqwestTransport>,
    compact: CompactClient<codex_api::ReqwestTransport>,
    websocket: ResponsesWebsocketClient,
    websocket_state: Mutex<WebsocketState>,
    http_client_factory: HttpClientFactory,
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

#[derive(Default)]
struct WebsocketState {
    connection: Option<ResponsesWebsocketConnection>,
    last_request: Option<ResponsesApiRequest>,
    last_response: Option<LastResponse>,
    fallback_active: bool,
}

struct LastResponse {
    id: String,
    items: Vec<ResponseItem>,
}

pub(crate) struct CodexTurn {
    session: Arc<CodexSession>,
    turn_state: Arc<OnceLock<String>>,
}

struct CompletionReadError {
    error: Error,
    visible_output: bool,
    report_fallback: bool,
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
        let http_client_factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
        let session = Arc::new(CodexSession {
            responses: ResponsesClient::new(
                codex_api::ReqwestTransport::from_http_client(create_client()),
                provider.clone(),
                api_auth.clone(),
            ),
            compact: CompactClient::new(
                codex_api::ReqwestTransport::from_http_client(create_client()),
                provider.clone(),
                api_auth.clone(),
            ),
            websocket: ResponsesWebsocketClient::new(provider.clone(), api_auth),
            websocket_state: Mutex::new(WebsocketState::default()),
            http_client_factory,
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
        let mut request = completion_request(model, reasoning_effort, events, prompt_cache_key)?;
        request.client_metadata = Some(HashMap::from([
            ("session_id".to_owned(), self.session.session_id.clone()),
            ("thread_id".to_owned(), self.session.session_id.clone()),
        ]));
        match self.complete_websocket(&request, updates).await {
            Ok(completion) => Ok(completion),
            Err(failure) if !failure.visible_output => {
                if failure.report_fallback {
                    tracing::warn!(
                        error = %format!("{:#}", failure.error),
                        session_id = %self.session.session_id,
                        "Codex websocket failed before emitting output; falling back to SSE"
                    );
                }
                self.complete_sse(request, updates).await
            }
            Err(failure) => Err(failure.error),
        }
    }

    async fn complete_websocket(
        &self,
        request: &ResponsesApiRequest,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> std::result::Result<ModelCompletion, CompletionReadError> {
        let mut state = self.session.websocket_state.lock().await;
        if state.fallback_active {
            return Err(CompletionReadError {
                error: anyhow::anyhow!("websocket fallback is active for this session"),
                visible_output: false,
                report_fallback: false,
            });
        }

        let closed = match state.connection.as_ref() {
            Some(connection) => connection.is_closed().await,
            None => true,
        };
        if closed {
            state.connection = None;
            state.last_request = None;
            state.last_response = None;
        }

        if state.connection.is_none() {
            let headers = self
                .websocket_headers()
                .map_err(|error| CompletionReadError {
                    error,
                    visible_output: false,
                    report_fallback: true,
                })?;
            match self
                .session
                .websocket
                .connect(
                    &self.session.http_client_factory,
                    headers,
                    HeaderMap::new(),
                    Some(self.turn_state.clone()),
                    None,
                )
                .await
            {
                Ok(connection) => {
                    state.connection = Some(connection);
                }
                Err(error) => {
                    state.fallback_active = is_upgrade_required(&error);
                    return Err(CompletionReadError {
                        error: Error::new(error).context("failed to connect Codex websocket"),
                        visible_output: false,
                        report_fallback: true,
                    });
                }
            }
        }

        let incremental = websocket_incremental_input(
            state.last_request.as_ref(),
            state.last_response.as_ref(),
            request,
        );
        let previous_response_id = incremental.as_ref().and_then(|_| {
            state
                .last_response
                .as_ref()
                .map(|response| response.id.clone())
        });
        let input = incremental.as_deref().unwrap_or(&request.input);
        let mut client_metadata = request.client_metadata.clone().unwrap_or_default();
        if let Some(turn_state) = self.turn_state.get() {
            client_metadata.insert("x-codex-turn-state".to_owned(), turn_state.clone());
        }
        let ws_request = ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
            previous_response_id,
            input,
            client_metadata: Some(client_metadata),
            ..ResponseCreateWsRequest::from(request)
        });
        let connection = state
            .connection
            .as_ref()
            .expect("connection was established");
        let stream = match connection
            .stream_request(
                ws_request,
                state.last_request.is_some(),
                Some(self.turn_state.clone()),
            )
            .await
        {
            Ok(stream) => stream,
            Err(error) => {
                state.connection = None;
                state.last_request = None;
                state.last_response = None;
                state.fallback_active = is_upgrade_required(&error);
                return Err(CompletionReadError {
                    error: Error::new(error).context("failed to start Codex websocket request"),
                    visible_output: false,
                    report_fallback: true,
                });
            }
        };

        match read_completion(stream, updates).await {
            Ok((completion, response)) => {
                state.last_request = Some(request.clone());
                state.last_response = Some(response);
                Ok(completion)
            }
            Err(failure) => {
                state.connection = None;
                state.last_request = None;
                state.last_response = None;
                Err(failure)
            }
        }
    }

    async fn complete_sse(
        &self,
        request: ResponsesApiRequest,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ModelCompletion> {
        let stream = self
            .session
            .responses
            .stream_request(
                request,
                ResponsesOptions {
                    session_id: Some(self.session.session_id.clone()),
                    thread_id: Some(self.session.session_id.clone()),
                    compression: Compression::None,
                    turn_state: Some(self.turn_state.clone()),
                    ..Default::default()
                },
            )
            .await
            .context("Codex request failed")?;
        read_completion(stream, updates)
            .await
            .map(|(completion, _)| completion)
            .map_err(|failure| failure.error)
    }

    fn websocket_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let session_id = HeaderValue::from_str(&self.session.session_id)
            .context("model session ID is not a valid header value")?;
        headers.insert("session-id", session_id.clone());
        headers.insert("thread-id", session_id.clone());
        headers.insert("x-client-request-id", session_id);
        headers.insert("originator", HeaderValue::from_static("atra"));
        headers.insert("openai-beta", HeaderValue::from_static(WEBSOCKET_BETA));
        Ok(headers)
    }

    pub(super) async fn compact(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<Vec<ResponseItem>> {
        self.session.touch();
        let mut state = self.session.websocket_state.lock().await;
        state.connection = None;
        state.last_request = None;
        state.last_response = None;
        drop(state);
        let input = model_input(events)?;
        let mut headers = HeaderMap::new();
        let session_id = HeaderValue::from_str(&self.session.session_id)
            .context("model session ID is not a valid header value")?;
        headers.insert("session-id", session_id.clone());
        headers.insert("thread-id", session_id.clone());
        headers.insert("x-client-request-id", session_id);
        self.session
            .compact
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
                headers,
                Duration::from_secs(300),
                Some(self.turn_state.as_ref()),
            )
            .await
            .context("Codex compaction failed")
    }
}

fn is_upgrade_required(error: &ApiError) -> bool {
    matches!(
        error,
        ApiError::Transport(TransportError::Http { status, .. })
            if *status == http::StatusCode::UPGRADE_REQUIRED
    )
}

async fn read_completion(
    mut stream: ResponseStream,
    updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
) -> std::result::Result<(ModelCompletion, LastResponse), CompletionReadError> {
    let mut responses = Vec::new();
    let mut response_id = None;
    let mut response_items = Vec::new();
    let mut reasoning = Vec::new();
    let mut token_usage = None;
    let mut rate_limits = Vec::new();
    let mut visible_output = false;

    while let Some(event) = stream.next().await {
        let event = event.map_err(|error| CompletionReadError {
            error: Error::new(error).context("Codex response stream failed"),
            visible_output,
            report_fallback: true,
        })?;
        match event {
            ResponseEvent::OutputTextDelta(delta) => {
                if let Some(updates) = updates {
                    visible_output |= updates
                        .send(ModelStreamEvent::AssistantDelta(delta))
                        .is_ok();
                }
            }
            ResponseEvent::ReasoningSummaryDelta { delta, .. } => {
                if let Some(updates) = updates {
                    visible_output |= updates
                        .send(ModelStreamEvent::ReasoningSummaryDelta(delta))
                        .is_ok();
                }
            }
            ResponseEvent::ReasoningSummaryPartAdded { .. } => {
                if let Some(updates) = updates {
                    visible_output |= updates
                        .send(ModelStreamEvent::ReasoningSummaryPartAdded)
                        .is_ok();
                }
            }
            ResponseEvent::OutputItemAdded(ResponseItem::CustomToolCall {
                id: Some(item_id),
                name,
                ..
            }) => {
                if let Some(updates) = updates {
                    visible_output |= updates
                        .send(ModelStreamEvent::ToolCallStarted {
                            item_id: item_id.to_string(),
                            name,
                        })
                        .is_ok();
                }
            }
            ResponseEvent::ToolCallInputDelta { item_id, delta, .. } => {
                if let Some(updates) = updates {
                    visible_output |= updates
                        .send(ModelStreamEvent::ToolCallDelta { item_id, delta })
                        .is_ok();
                }
            }
            ResponseEvent::OutputItemDone(item) => {
                response_items.push(item.clone());
                if matches!(item, ResponseItem::Reasoning { .. }) {
                    reasoning.push(item);
                } else if let Some(item_response) =
                    response_from_item(item).map_err(|error| CompletionReadError {
                        error,
                        visible_output,
                        report_fallback: true,
                    })?
                {
                    responses.push(item_response);
                }
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
                rate_limits.push(snapshot);
            }
            _ => {}
        }
    }

    if responses.is_empty() {
        return Err(CompletionReadError {
            error: anyhow::anyhow!(
                "Codex response ended without an assistant message or tool call"
            ),
            visible_output,
            report_fallback: true,
        });
    }
    let response_id = response_id.ok_or_else(|| CompletionReadError {
        error: anyhow::anyhow!("Codex response ended without a response ID"),
        visible_output,
        report_fallback: true,
    })?;
    Ok((
        ModelCompletion {
            responses,
            reasoning,
            token_usage,
            rate_limits,
        },
        LastResponse {
            id: response_id,
            items: response_items,
        },
    ))
}

fn websocket_incremental_input(
    previous_request: Option<&ResponsesApiRequest>,
    previous_response: Option<&LastResponse>,
    request: &ResponsesApiRequest,
) -> Option<Vec<ResponseItem>> {
    let previous_request = previous_request?;
    let previous_response = previous_response?;
    if !request_properties_match(previous_request, request) {
        return None;
    }

    let baseline_len = previous_request
        .input
        .len()
        .checked_add(previous_response.items.len())?;
    if request.input.len() < baseline_len {
        return None;
    }
    let (prefix, incremental) = request.input.split_at(baseline_len);
    let (request_prefix, response_prefix) = prefix.split_at(previous_request.input.len());
    if request_prefix != previous_request.input
        || !response_prefix
            .iter()
            .zip(&previous_response.items)
            .all(|(current, previous)| response_items_equal(current, previous))
    {
        return None;
    }
    Some(incremental.to_vec())
}

fn request_properties_match(previous: &ResponsesApiRequest, current: &ResponsesApiRequest) -> bool {
    let Ok(mut previous) = serde_json::to_value(previous) else {
        return false;
    };
    let Ok(mut current) = serde_json::to_value(current) else {
        return false;
    };
    for request in [&mut previous, &mut current] {
        if let Some(request) = request.as_object_mut() {
            request.remove("input");
            request.remove("client_metadata");
        }
    }
    previous == current
}

fn response_items_equal(current: &ResponseItem, previous: &ResponseItem) -> bool {
    if current == previous {
        return true;
    }
    let mut current = current.clone();
    current.set_id(None);
    current.clear_internal_chat_message_metadata_passthrough();
    let mut previous = previous.clone();
    previous.set_id(None);
    previous.clear_internal_chat_message_metadata_passthrough();
    current == previous
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
        parallel_tool_calls: true,
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
                            role: "user".to_owned(),
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
                            role: "user".to_owned(),
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
                    EventKind::Compaction
                    | EventKind::ModelRequest
                    | EventKind::TokenUsage
                    | EventKind::RateLimits => return None,
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
