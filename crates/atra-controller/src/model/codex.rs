use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, OnceLock, RwLock as StdRwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use codex_api::{
    AuthProvider, CompactClient, CompactionInput, ModelsClient, OpenAiVerbosity, Reasoning,
    ResponseEvent, ResponseStream, ResponsesApiRequest, ResponsesApiTools, ResponsesClient,
    TextControls,
};
use codex_backend_client::Client as BackendClient;
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_login::{
    AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthManager, CodexAuth, RefreshTokenError,
    UnauthorizedRecovery, default_client::create_client,
};
use codex_model_provider_info::{CHATGPT_CODEX_BASE_URL, ModelProviderInfo};
use codex_protocol::error::CodexErr;
use codex_protocol::models::{
    ContentItem, FunctionCallOutputPayload, ResponseInputItem, ResponseItem,
};
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::{config_types::ReasoningSummary, protocol::RateLimitSnapshot};
use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue};
use rand::Rng;
use serde_json::{json, value::RawValue};
use tokio::sync::{Mutex, RwLock};

use atra_protocol::{Model, Runner};

use super::{
    ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ModelResponse,
    ModelResponseMetadata, ModelSession, ModelStreamEvent, ModelTool, ProviderOutput,
};
use crate::storage::Event;
use atra_protocol::{InstructionEvent, RunnersEvent, ThreadEventData, ToolResultEvent};

const SESSION_IDLE_TTL: Duration = Duration::from_secs(60 * 60);
const PROVIDER_ID: &str = "codex";

pub(crate) struct CodexProvider {
    auth: Arc<AuthManager>,
    models: RwLock<Option<Vec<Model>>>,
    sessions: Mutex<HashMap<String, Arc<CodexSession>>>,
    rate_limits: Arc<RwLock<Vec<RateLimitSnapshot>>>,
}

struct CodexSession {
    responses: ResponsesClient<codex_api::ReqwestTransport>,
    compact: CompactClient<codex_api::ReqwestTransport>,
    auth: Arc<BearerAuth>,
    auth_manager: Arc<AuthManager>,
    session_id: String,
    stream_max_retries: u64,
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

    async fn recover_unauthorized(&self, recovery: &mut UnauthorizedRecovery) -> Result<bool> {
        if !recovery.has_next() {
            return Ok(false);
        }
        match recovery.next().await {
            Ok(_) => {}
            Err(RefreshTokenError::Permanent(error)) => {
                return Err(CodexErr::RefreshTokenFailed(error).into());
            }
            Err(RefreshTokenError::Transient(error)) => return Err(CodexErr::Io(error).into()),
        }
        let auth = self
            .auth_manager
            .auth_cached()
            .filter(CodexAuth::is_chatgpt_auth)
            .context("Codex login required; run `atra codex login`")?;
        self.auth.replace(&auth)?;
        Ok(true)
    }
}

pub(crate) struct CodexTurn {
    session: Arc<CodexSession>,
    turn_state: Arc<OnceLock<String>>,
    rate_limits: Arc<RwLock<Vec<RateLimitSnapshot>>>,
    retries: Arc<Mutex<u64>>,
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

    pub(crate) async fn login_status(&self) -> Option<Option<String>> {
        self.auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .map(|auth| auth.get_account_email())
    }

    pub(crate) async fn reload_auth(&self) {
        self.auth.reload().await;
        *self.models.write().await = None;
        self.sessions.lock().await.clear();
        self.rate_limits.write().await.clear();
    }

    pub(crate) async fn logout(&self) -> Result<()> {
        self.auth
            .logout_with_revoke()
            .await
            .context("failed to log out of Codex")?;
        *self.models.write().await = None;
        self.sessions.lock().await.clear();
        self.rate_limits.write().await.clear();
        Ok(())
    }

    pub(crate) async fn rate_limits(&self) -> Result<Vec<RateLimitSnapshot>> {
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
                retries: Arc::new(Mutex::new(0)),
            });
        }
        drop(sessions);
        let auth = self
            .auth
            .auth()
            .await
            .filter(CodexAuth::is_chatgpt_auth)
            .context("Codex login required; run `atra codex login`")?;
        let provider = ModelProviderInfo::create_openai_provider(None);
        let stream_max_retries = provider.stream_max_retries();
        let provider = provider
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
                api_auth.clone(),
            ),
            auth: api_auth,
            auth_manager: self.auth.clone(),
            session_id,
            stream_max_retries,
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
            retries: Arc::new(Mutex::new(0)),
        })
    }
}

#[async_trait]
impl ModelProvider for CodexProvider {
    async fn models(&self) -> Result<Vec<Model>> {
        self.models().await
    }

    async fn start_turn(&self, session_id: &str) -> Result<Box<dyn ModelSession + '_>> {
        Ok(Box::new(self.start_turn(session_id.to_owned()).await?))
    }

    fn context_tokens(&self, events: &[Event]) -> Result<usize> {
        context_tokens(events)
    }
}

#[async_trait]
impl ModelSession for CodexTurn {
    async fn stream(&self, request: &ModelRequest<'_>) -> Result<ModelEventStream> {
        self.stream(request).await
    }

    async fn compact(&self, request: &ModelRequest<'_>) -> Result<Option<ProviderOutput>> {
        self.compact(request).await
    }
}

impl CodexTurn {
    async fn stream(&self, request: &ModelRequest<'_>) -> Result<ModelEventStream> {
        self.session.touch();
        let request = completion_request(request)?;
        let mut headers = HeaderMap::new();
        let session_id = HeaderValue::from_str(&self.session.session_id)
            .context("model session ID is not a valid header value")?;
        headers.insert("session-id", session_id.clone());
        headers.insert("thread-id", session_id.clone());
        headers.insert("x-client-request-id", session_id);
        let state = CodexModelStream {
            session: self.session.clone(),
            turn_state: self.turn_state.clone(),
            latest_rate_limits: self.rate_limits.clone(),
            request,
            headers,
            stream: None,
            retries: self.retries.clone(),
            auth_recovery: self.session.auth_manager.unauthorized_recovery(),
            has_response: false,
            response_id: None,
            token_usage: None,
            rate_limits: Vec::new(),
            pending: VecDeque::new(),
            finished: false,
        };
        Ok(stream::unfold(state, |mut state| async move {
            state.next_event().await.map(|event| (event, state))
        })
        .boxed())
    }

    async fn compact(&self, request: &ModelRequest<'_>) -> Result<Option<ProviderOutput>> {
        self.session.touch();
        let request = compaction_request(request)?;
        let mut headers = HeaderMap::new();
        let session_id = HeaderValue::from_str(&self.session.session_id)
            .context("model session ID is not a valid header value")?;
        headers.insert("session-id", session_id.clone());
        headers.insert("thread-id", session_id.clone());
        headers.insert("x-client-request-id", session_id);
        let mut auth_recovery = self.session.auth_manager.unauthorized_recovery();
        let items = loop {
            match self
                .session
                .compact
                .compact(
                    request.clone(),
                    headers.clone(),
                    Duration::from_secs(300),
                    Some(self.turn_state.as_ref()),
                )
                .await
            {
                Ok(items) => break items,
                Err(error) => {
                    if is_unauthorized(&error)
                        && self
                            .session
                            .recover_unauthorized(&mut auth_recovery)
                            .await?
                    {
                        continue;
                    }
                    return Err(codex_api::map_api_error(error).into());
                }
            }
        };
        if items.is_empty() {
            return Ok(None);
        }
        Ok(Some(ProviderOutput {
            provider: PROVIDER_ID.to_owned(),
            data: serde_json::to_value(items).context("failed to encode Codex compaction")?,
        }))
    }
}

struct CodexModelStream {
    session: Arc<CodexSession>,
    turn_state: Arc<OnceLock<String>>,
    latest_rate_limits: Arc<RwLock<Vec<RateLimitSnapshot>>>,
    request: serde_json::Value,
    headers: HeaderMap,
    stream: Option<ResponseStream>,
    retries: Arc<Mutex<u64>>,
    auth_recovery: UnauthorizedRecovery,
    has_response: bool,
    response_id: Option<String>,
    token_usage: Option<codex_protocol::protocol::TokenUsage>,
    rate_limits: Vec<RateLimitSnapshot>,
    pending: VecDeque<Result<ModelEvent>>,
    finished: bool,
}

impl CodexModelStream {
    async fn next_event(&mut self) -> Option<Result<ModelEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                self.finished |= event.is_err();
                return Some(event);
            }
            if self.finished {
                return None;
            }
            if self.stream.is_none() {
                match self
                    .session
                    .responses
                    .stream(
                        self.request.clone(),
                        self.headers.clone(),
                        codex_api::Compression::None,
                        Some(self.turn_state.clone()),
                    )
                    .await
                {
                    Ok(stream) => self.stream = Some(stream),
                    Err(error) => {
                        if is_unauthorized(&error)
                            && match self
                                .session
                                .recover_unauthorized(&mut self.auth_recovery)
                                .await
                            {
                                Ok(recovered) => recovered,
                                Err(error) => return Some(self.fail(error)),
                            }
                        {
                            continue;
                        }
                        let error = anyhow::Error::new(codex_api::map_api_error(error));
                        return Some(self.retry_or_fail(error).await);
                    }
                }
            }

            let event = self.stream.as_mut().unwrap().next().await;
            let event = match event {
                Some(Ok(event)) => event,
                Some(Err(error)) => {
                    self.stream = None;
                    let error = anyhow::Error::new(codex_api::map_api_error(error));
                    return Some(self.retry_or_fail(error).await);
                }
                None => {
                    self.finished = true;
                    if !self.has_response {
                        return Some(Err(anyhow::anyhow!(
                            "Codex response ended without an assistant message or tool call"
                        )));
                    }
                    let token_usage = match self.token_usage.take().map(serde_json::to_value) {
                        Some(Ok(usage)) => Some(usage),
                        Some(Err(error)) => {
                            return Some(Err(error).context("failed to encode Codex token usage"));
                        }
                        None => None,
                    };
                    let rate_limits = match self
                        .rate_limits
                        .drain(..)
                        .map(serde_json::to_value)
                        .collect::<serde_json::Result<_>>()
                    {
                        Ok(rate_limits) => rate_limits,
                        Err(error) => {
                            return Some(Err(error).context("failed to encode Codex rate limits"));
                        }
                    };
                    *self.retries.lock().await = 0;
                    return Some(Ok(ModelEvent::Completed {
                        metadata: self.response_id.take().map(|response_id| {
                            ModelResponseMetadata {
                                provider: PROVIDER_ID.to_owned(),
                                response_id,
                            }
                        }),
                        token_usage,
                        rate_limits,
                    }));
                }
            };

            match event {
                ResponseEvent::OutputTextDelta(delta) => {
                    return Some(Ok(ModelEvent::Update(ModelStreamEvent::AssistantDelta(
                        delta,
                    ))));
                }
                ResponseEvent::ReasoningSummaryDelta { delta, .. } => {
                    return Some(Ok(ModelEvent::Update(
                        ModelStreamEvent::ReasoningSummaryDelta(delta),
                    )));
                }
                ResponseEvent::ReasoningSummaryPartAdded { .. } => {
                    return Some(Ok(ModelEvent::Update(
                        ModelStreamEvent::ReasoningSummaryPartAdded,
                    )));
                }
                ResponseEvent::OutputItemAdded(ResponseItem::WebSearchCall {
                    id: Some(item_id),
                    action,
                    ..
                }) => {
                    let action = match action.map(serde_json::to_value).transpose() {
                        Ok(action) => action,
                        Err(error) => {
                            return Some(
                                self.fail(
                                    anyhow::Error::new(error)
                                        .context("failed to encode live web search action"),
                                ),
                            );
                        }
                    };
                    return Some(Ok(ModelEvent::Update(ModelStreamEvent::WebSearchUpdate {
                        item_id: item_id.to_string(),
                        action,
                    })));
                }
                ResponseEvent::OutputItemAdded(ResponseItem::CustomToolCall {
                    id: Some(item_id),
                    name,
                    ..
                }) => {
                    return Some(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
                        item_id: item_id.to_string(),
                        name,
                    })));
                }
                ResponseEvent::ToolCallInputDelta { item_id, delta, .. } => {
                    return Some(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
                        item_id,
                        delta,
                    })));
                }
                ResponseEvent::OutputItemDone(item) => {
                    let output = match serde_json::to_value([&item]) {
                        Ok(data) => ProviderOutput {
                            provider: PROVIDER_ID.to_owned(),
                            data,
                        },
                        Err(error) => {
                            return Some(
                                self.fail(
                                    anyhow::Error::new(error)
                                        .context("failed to encode completed Codex output item"),
                                ),
                            );
                        }
                    };
                    let response = match response_from_item(item.clone()) {
                        Ok(response) => response,
                        Err(error) => return Some(self.fail(error)),
                    };
                    if let Some(response) = &response {
                        self.has_response |= !matches!(response, ModelResponse::Reasoning { .. });
                    }
                    let completed = ModelEvent::OutputItemDone { output, response };
                    if let ResponseItem::WebSearchCall {
                        id: Some(item_id),
                        action,
                        ..
                    } = &item
                    {
                        match action.as_ref().map(serde_json::to_value).transpose() {
                            Ok(action) => {
                                self.pending.push_back(Ok(completed));
                                return Some(Ok(ModelEvent::Update(
                                    ModelStreamEvent::WebSearchUpdate {
                                        item_id: item_id.to_string(),
                                        action,
                                    },
                                )));
                            }
                            Err(error) => {
                                self.pending.push_back(Err(anyhow::Error::new(error)
                                    .context("failed to encode live web search action")));
                            }
                        }
                    }
                    return Some(Ok(completed));
                }
                ResponseEvent::Completed {
                    response_id,
                    token_usage,
                    ..
                } => {
                    self.response_id = Some(response_id);
                    self.token_usage = token_usage;
                }
                ResponseEvent::RateLimits(snapshot) => {
                    update_rate_limit(&self.latest_rate_limits, snapshot.clone()).await;
                    self.rate_limits.push(snapshot);
                }
                _ => {}
            }
        }
    }

    fn fail(&mut self, error: anyhow::Error) -> Result<ModelEvent> {
        self.finished = true;
        Err(error)
    }

    async fn retry_or_fail(&mut self, error: anyhow::Error) -> Result<ModelEvent> {
        match completion_retry_event(
            error,
            &self.retries,
            self.session.stream_max_retries,
            &self.latest_rate_limits,
        )
        .await
        {
            Ok(event) => {
                self.finished = true;
                Ok(event)
            }
            Err(error) => self.fail(error),
        }
    }
}

async fn update_rate_limit(
    latest_rate_limits: &RwLock<Vec<RateLimitSnapshot>>,
    snapshot: RateLimitSnapshot,
) {
    let mut latest = latest_rate_limits.write().await;
    let limit_id = snapshot.limit_id.as_deref().unwrap_or("codex");
    if let Some(current) = latest
        .iter_mut()
        .find(|current| current.limit_id.as_deref().unwrap_or("codex") == limit_id)
    {
        *current = snapshot;
    } else {
        latest.push(snapshot);
    }
}

async fn completion_retry_event(
    error: anyhow::Error,
    retries: &Mutex<u64>,
    max_retries: u64,
    latest_rate_limits: &RwLock<Vec<RateLimitSnapshot>>,
) -> Result<ModelEvent> {
    let Some(codex_error) = error.downcast_ref::<CodexErr>() else {
        return Err(error);
    };
    if let CodexErr::UsageLimitReached(limit) = codex_error
        && let Some(snapshot) = limit.rate_limits.as_deref()
    {
        update_rate_limit(latest_rate_limits, snapshot.clone()).await;
    }
    let mut retries = retries.lock().await;
    if !codex_error.is_retryable() || *retries >= max_retries {
        return Err(error);
    }
    *retries += 1;
    let delay = match codex_error {
        CodexErr::Stream(_, Some(delay)) => *delay,
        _ => backoff(*retries),
    };
    Ok(ModelEvent::Retry {
        current: *retries,
        max: max_retries,
        delay,
    })
}

fn backoff(attempt: u64) -> Duration {
    let base_ms = 200.0 * 2.0_f64.powi(attempt.saturating_sub(1) as i32);
    Duration::from_millis((base_ms * rand::rng().random_range(0.9..1.1)) as u64)
}

fn is_unauthorized(error: &codex_api::ApiError) -> bool {
    matches!(
        error,
        codex_api::ApiError::Transport(codex_api::TransportError::Http { status, .. })
            if *status == http::StatusCode::UNAUTHORIZED
    )
}

fn completion_request(request: &ModelRequest<'_>) -> Result<serde_json::Value> {
    serde_json::to_value(ResponsesApiRequest {
        model: request.model.to_owned(),
        instructions: request.instructions.to_owned(),
        input: model_input(request.events)?,
        tools: Some(tool_definitions(request.tools)?),
        tool_choice: "auto".to_owned(),
        parallel_tool_calls: true,
        reasoning: Some(reasoning(request.reasoning_effort)?),
        store: false,
        stream: true,
        stream_options: None,
        include: vec!["reasoning.encrypted_content".to_owned()],
        service_tier: None,
        prompt_cache_key: Some(request.prompt_cache_key.to_owned()),
        text: Some(TextControls {
            verbosity: Some(OpenAiVerbosity::Low),
            format: None,
        }),
        client_metadata: Some(HashMap::from([
            ("session_id".to_owned(), request.prompt_cache_key.to_owned()),
            ("thread_id".to_owned(), request.prompt_cache_key.to_owned()),
        ])),
    })
    .context("failed to encode Codex request")
}

fn compaction_request(request: &ModelRequest<'_>) -> Result<serde_json::Value> {
    let input = model_input(request.events)?;
    serde_json::to_value(CompactionInput {
        model: request.model,
        input: &input,
        instructions: request.instructions,
        tools: Some(tool_definitions(request.tools)?),
        parallel_tool_calls: false,
        reasoning: Some(reasoning(request.reasoning_effort)?),
        service_tier: None,
        prompt_cache_key: Some(request.prompt_cache_key),
        text: Some(TextControls {
            verbosity: Some(OpenAiVerbosity::Low),
            format: None,
        }),
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

fn response_from_item(item: ResponseItem) -> Result<Option<ModelResponse>> {
    match item {
        ResponseItem::Message { content, phase, .. } => {
            let content = content
                .into_iter()
                .filter_map(|item| match item {
                    ContentItem::OutputText { text } => Some(text),
                    ContentItem::InputText { .. }
                    | ContentItem::InputImage { .. }
                    | ContentItem::InputAudio { .. } => None,
                })
                .collect::<String>();
            let phase = phase.map(|phase| match phase {
                codex_protocol::models::MessagePhase::Commentary => {
                    atra_protocol::AssistantMessagePhase::Commentary
                }
                codex_protocol::models::MessagePhase::FinalAnswer => {
                    atra_protocol::AssistantMessagePhase::FinalAnswer
                }
            });
            Ok((!content.is_empty()).then_some(ModelResponse::AssistantMessage { content, phase }))
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
        item @ ResponseItem::WebSearchCall { .. } => Ok(Some(ModelResponse::WebSearch {
            item: serde_json::to_value(item).context("failed to encode Codex web search")?,
        })),
        item @ ResponseItem::Reasoning { .. } => Ok(Some(ModelResponse::Reasoning {
            item: serde_json::to_value(item).context("failed to encode Codex reasoning")?,
        })),
        _ => Ok(None),
    }
}

struct BearerAuth {
    headers: StdRwLock<BearerAuthHeaders>,
}

struct BearerAuthHeaders {
    token: HeaderValue,
    account_id: Option<HeaderValue>,
}

impl BearerAuth {
    fn new(auth: &CodexAuth) -> Result<Self> {
        Ok(Self {
            headers: StdRwLock::new(BearerAuthHeaders::new(auth)?),
        })
    }

    fn replace(&self, auth: &CodexAuth) -> Result<()> {
        *self
            .headers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = BearerAuthHeaders::new(auth)?;
        Ok(())
    }
}

impl BearerAuthHeaders {
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
        let auth = self
            .headers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        headers.insert(http::header::AUTHORIZATION, auth.token.clone());
        if let Some(account_id) = &auth.account_id {
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
        let output = serde_json::from_value::<ProviderOutput>(match &events[index].data {
            ThreadEventData::Compaction(compaction) => compaction.items.clone(),
            _ => unreachable!(),
        })
        .context("stored compaction contains invalid provider output")?;
        anyhow::ensure!(
            output.provider == PROVIDER_ID,
            "stored compaction belongs to provider {}",
            output.provider
        );
        items.extend(
            serde_json::from_value::<Vec<ResponseItem>>(output.data)
                .context("stored compaction contains invalid Codex response items")?,
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
            let output = serde_json::from_value::<ProviderOutput>(event.output.clone())
                .context("stored model output contains invalid provider output")?;
            anyhow::ensure!(
                output.provider == PROVIDER_ID,
                "stored model output belongs to provider {}",
                output.provider
            );
            items.extend(
                serde_json::from_value::<Vec<ResponseItem>>(output.data)
                    .context("stored model output contains invalid Codex response items")?,
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

fn tool_definitions(tools: &[ModelTool]) -> Result<ResponsesApiTools> {
    let tools = tools
        .iter()
        .map(|tool| match tool {
            ModelTool::WebSearch => json!({
                "type": "web_search",
                "external_web_access": true
            }),
            ModelTool::Custom {
                name,
                description,
                format,
            } => json!({
                "type": "custom",
                "name": name,
                "description": description,
                "format": {
                    "type": "grammar",
                    "syntax": format.syntax,
                    "definition": format.definition,
                },
            }),
        })
        .collect::<Vec<_>>();
    let raw = RawValue::from_string(serde_json::to_string(&tools)?)
        .context("failed to encode tool schemas")?;
    Ok(Arc::<RawValue>::from(raw).into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn completion_request_contains_exact_context_and_omits_observer_events() {
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
                    context_window: None,
                }),
            },
        ];

        let tools = crate::tools::model_tools();
        let request = completion_request(&ModelRequest {
            model: "model",
            reasoning_effort: "medium",
            instructions: super::super::BASE_INSTRUCTIONS,
            tools: &tools,
            events: &events,
            prompt_cache_key: "cache",
        })
        .unwrap();
        let snapshot = serde_json::to_value(request).unwrap();

        assert_eq!(snapshot["model"], "model");
        assert_eq!(snapshot["prompt_cache_key"], "cache");
        assert_eq!(snapshot["text"], json!({"verbosity": "low"}));
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
