use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use rand::Rng;
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue},
};

use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, mpsc};

use atra_protocol::{InstructionEvent, Model, RunnersEvent, ThreadEventData, ToolResultEvent};

use super::{
    ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ModelResponse,
    ModelResponseMetadata, ModelSession, ModelStreamEvent, ModelTool, ProviderLoginStatus,
    ProviderOutput,
    codex_auth::{Auth, AuthManager},
    format_runners,
};
use crate::storage::Event;

const PROVIDER_ID: &str = super::CODEX_PROVIDER;
const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const STREAM_MAX_RETRIES: u64 = 5;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
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
    retries: Mutex<u64>,
    last_used: std::sync::Mutex<Instant>,
}

pub(crate) struct CodexTurn {
    session: Arc<CodexSession>,
    turn_state: Arc<Mutex<Option<String>>>,
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

    async fn rate_limits_inner(&self) -> Result<Vec<Value>> {
        let auth = self
            .auth
            .auth()
            .await?
            .context("Codex login required; run `atra codex login`")?;
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
            .context("Codex login required; run `atra codex login`")?;
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
impl ModelProvider for CodexProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
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

    async fn execute_tool(&self, _name: &str, _arguments: &Value) -> Result<Option<Value>> {
        Ok(None)
    }

    async fn start_turn(&self, session_id: &str) -> Result<Box<dyn ModelSession + '_>> {
        anyhow::ensure!(
            self.auth.auth().await?.is_some(),
            "Codex login required; run `atra codex login`"
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
                    retries: Mutex::new(0),
                    last_used: std::sync::Mutex::new(Instant::now()),
                })
            })
            .clone();
        session.touch();
        Ok(Box::new(CodexTurn {
            session,
            turn_state: Arc::new(Mutex::new(None)),
        }))
    }

    fn context_tokens(&self, events: &[Event]) -> Result<usize> {
        context_tokens(events)
    }
}

#[async_trait]
impl ModelSession for CodexTurn {
    async fn stream(&self, request: &ModelRequest<'_>) -> Result<ModelEventStream> {
        self.session.touch();
        let body = completion_request(request)?;
        let session = self.session.clone();
        let turn_state = self.turn_state.clone();
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(async move {
            stream_response(session, turn_state, body, sender).await;
        });
        Ok(stream::unfold(receiver, |mut receiver| async {
            receiver.recv().await.map(|event| (event, receiver))
        })
        .boxed())
    }

    async fn compact(&self, request: &ModelRequest<'_>) -> Result<Option<ProviderOutput>> {
        self.session.touch();
        let body = compaction_request(request)?;
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.session.send(
                reqwest::Method::POST,
                &format!("{BASE_URL}/responses"),
                Some(body),
                &self.turn_state,
            ),
        )
        .await
        .context("Codex compaction request timed out")??;
        let item = decode_compaction_stream(response).await?;
        Ok(Some(ProviderOutput {
            provider: PROVIDER_ID.to_owned(),
            data: Value::Array(vec![item]),
        }))
    }
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
            .context("Codex login required; run `atra codex login`")?;
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
            .context("Codex login required; run `atra codex login`")?;
    }
    unreachable!()
}

async fn stream_response(
    session: Arc<CodexSession>,
    turn_state: Arc<Mutex<Option<String>>>,
    body: Value,
    sender: mpsc::Sender<Result<ModelEvent>>,
) {
    let mut attempt = 0;
    loop {
        let mut emitted = false;
        let result =
            stream_response_once(&session, &turn_state, body.clone(), &sender, &mut emitted).await;
        match result {
            Ok(()) => return,
            Err(error) if should_retry_stream(&error, emitted, attempt) => {
                attempt += 1;
                *session.retries.lock().await += 1;
                let delay = backoff(attempt);
                if sender
                    .send(Ok(ModelEvent::Update(ModelStreamEvent::Retry {
                        summary: error.to_string(),
                        current: attempt,
                        max: STREAM_MAX_RETRIES,
                    })))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = sender.closed() => return,
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
        }
    }
}

async fn stream_response_once(
    session: &CodexSession,
    turn_state: &Mutex<Option<String>>,
    body: Value,
    sender: &mpsc::Sender<Result<ModelEvent>>,
    emitted: &mut bool,
) -> Result<()> {
    let url = format!("{BASE_URL}/responses");
    let response = tokio::select! {
        () = sender.closed() => return Ok(()),
        response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            session.send(
                reqwest::Method::POST,
                &url,
                Some(body),
                turn_state,
            ),
        ) => response
            .map_err(|error| RetryableStreamError(error.into()))??,
    };
    if !response.status().is_success() {
        let status = response.status();
        let error = response_error(response, "Codex response").await;
        return if retryable_status(status) {
            Err(RetryableStreamError(error).into())
        } else {
            Err(error)
        };
    }
    let header_limits = rate_limit_headers(response.headers());
    if !header_limits.is_empty() {
        let mut latest_rate_limits = session.rate_limits.write().await;
        merge_rate_limits(&mut latest_rate_limits, header_limits.clone());
    }
    let mut bytes = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut response_id = None;
    let mut token_usage = None;
    let mut rate_limits = header_limits;
    let mut saw_response = false;
    let mut completed = false;
    let mut has_response = false;

    'stream: loop {
        let chunk = tokio::select! {
            () = sender.closed() => return Ok(()),
            chunk = tokio::time::timeout(STREAM_IDLE_TIMEOUT, bytes.next()) => chunk
                .map_err(|error| RetryableStreamError(error.into()))?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.context("failed to read Codex response stream")?;
        buffer.extend_from_slice(&chunk);
        while let Some(frame) = next_sse_frame(&mut buffer)? {
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                break 'stream;
            }
            let event: Value = serde_json::from_str(&data).context("invalid Codex SSE event")?;
            saw_response = true;
            if handle_sse_event(
                event,
                sender,
                &mut response_id,
                &mut token_usage,
                &mut rate_limits,
                emitted,
                &mut has_response,
            )
            .await?
            {
                completed = true;
            }
        }
    }
    if completed {
        ensure_completed_response(has_response)?;
        *session.retries.lock().await = 0;
        let mut latest_rate_limits = session.rate_limits.write().await;
        merge_rate_limits(&mut latest_rate_limits, rate_limits);
        let rate_limits = latest_rate_limits.clone();
        drop(latest_rate_limits);
        let _ = sender
            .send(Ok(ModelEvent::Completed {
                metadata: response_id.map(|response_id| ModelResponseMetadata {
                    provider: PROVIDER_ID.to_owned(),
                    response_id,
                }),
                token_usage,
                rate_limits,
            }))
            .await;
        return Ok(());
    }
    if saw_response {
        return Err(RetryableStreamError(anyhow::anyhow!(
            "Codex response stream ended before response.completed"
        ))
        .into());
    }
    Err(RetryableStreamError(anyhow::anyhow!(
        "Codex response stream ended without events"
    ))
    .into())
}

fn ensure_completed_response(has_response: bool) -> Result<()> {
    anyhow::ensure!(
        has_response,
        "Codex response completed without an assistant message or tool call"
    );
    Ok(())
}

async fn handle_sse_event(
    event: Value,
    sender: &mpsc::Sender<Result<ModelEvent>>,
    response_id: &mut Option<String>,
    token_usage: &mut Option<Value>,
    rate_limits: &mut Vec<Value>,
    emitted: &mut bool,
    has_response: &mut bool,
) -> Result<bool> {
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let update = match kind {
        "response.output_text.delta" => event["delta"]
            .as_str()
            .map(|value| ModelStreamEvent::AssistantDelta(value.to_owned())),
        "response.reasoning_summary_text.delta" => event["delta"]
            .as_str()
            .map(|value| ModelStreamEvent::ReasoningSummaryDelta(value.to_owned())),
        "response.reasoning_summary_part.added" => {
            Some(ModelStreamEvent::ReasoningSummaryPartAdded)
        }
        "response.custom_tool_call_input.delta" => Some(ModelStreamEvent::ToolCallDelta {
            item_id: event["item_id"].as_str().unwrap_or_default().to_owned(),
            delta: event["delta"].as_str().unwrap_or_default().to_owned(),
        }),
        "response.output_item.added" => {
            let item = &event["item"];
            match item["type"].as_str() {
                Some("custom_tool_call") | Some("function_call") => {
                    Some(ModelStreamEvent::ToolCallStarted {
                        item_id: item["id"].as_str().unwrap_or_default().to_owned(),
                        call_id: item["call_id"].as_str().map(str::to_owned),
                        name: item["name"].as_str().unwrap_or_default().to_owned(),
                    })
                }
                Some("web_search_call") => Some(ModelStreamEvent::WebSearchUpdate {
                    item_id: item["id"].as_str().unwrap_or_default().to_owned(),
                    action: item.get("action").filter(|value| !value.is_null()).cloned(),
                }),
                _ => None,
            }
        }
        "response.web_search_call.in_progress"
        | "response.web_search_call.searching"
        | "response.web_search_call.completed" => Some(ModelStreamEvent::WebSearchUpdate {
            item_id: event["item_id"].as_str().unwrap_or_default().to_owned(),
            action: event
                .get("action")
                .filter(|value| !value.is_null())
                .cloned(),
        }),
        _ => None,
    };
    if let Some(update) = update {
        *emitted = true;
        sender
            .send(Ok(ModelEvent::Update(update)))
            .await
            .map_err(|_| anyhow::anyhow!("model stream receiver dropped"))?;
    }
    match kind {
        "response.output_item.done" => {
            let item = event["item"].clone();
            if item["type"] == "web_search_call" {
                *emitted = true;
                sender
                    .send(Ok(ModelEvent::Update(ModelStreamEvent::WebSearchUpdate {
                        item_id: item["id"].as_str().unwrap_or_default().to_owned(),
                        action: item.get("action").filter(|value| !value.is_null()).cloned(),
                    })))
                    .await
                    .map_err(|_| anyhow::anyhow!("model stream receiver dropped"))?;
            }
            let response = response_from_item(&item)?;
            if matches!(
                response,
                Some(
                    ModelResponse::AssistantMessage { .. }
                        | ModelResponse::ToolCall { .. }
                        | ModelResponse::CustomToolCall { .. }
                )
            ) {
                *has_response = true;
            }
            *emitted = true;
            sender
                .send(Ok(ModelEvent::OutputItemDone {
                    response,
                    output: ProviderOutput {
                        provider: PROVIDER_ID.to_owned(),
                        data: Value::Array(vec![item]),
                    },
                }))
                .await
                .map_err(|_| anyhow::anyhow!("model stream receiver dropped"))?;
        }
        "response.completed" => {
            let response = &event["response"];
            *response_id = response["id"].as_str().map(str::to_owned);
            *token_usage = response
                .get("usage")
                .filter(|value| !value.is_null())
                .map(normalize_token_usage);
            if let Some(limits) = event.get("rate_limits") {
                *rate_limits = rate_limit_snapshots(limits);
            }
            return Ok(true);
        }
        "codex.rate_limits" => {
            let limits = event.get("rate_limits").unwrap_or(&event);
            merge_rate_limits(rate_limits, rate_limit_snapshots(limits));
        }
        "error" | "response.failed" => {
            return Err(sse_failure(&event));
        }
        _ => {}
    }
    Ok(false)
}

fn normalize_token_usage(usage: &Value) -> Value {
    json!({
        "input_tokens": usage.get("input_tokens").cloned().unwrap_or(Value::Null),
        "cached_input_tokens": usage
            .pointer("/input_tokens_details/cached_tokens")
            .cloned()
            .unwrap_or(Value::Null),
        "cache_write_input_tokens": usage
            .pointer("/input_tokens_details/cache_write_tokens")
            .cloned()
            .unwrap_or(Value::Null),
        "output_tokens": usage.get("output_tokens").cloned().unwrap_or(Value::Null),
        "reasoning_output_tokens": usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .cloned()
            .unwrap_or(Value::Null),
        "total_tokens": usage.get("total_tokens").cloned().unwrap_or(Value::Null),
    })
}

fn completion_request(request: &ModelRequest<'_>) -> Result<Value> {
    Ok(json!({
        "model": request.model,
        "instructions": request.instructions,
        "input": model_input(request.events)?,
        "tools": tool_definitions(request.tools),
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": {"effort": request.reasoning_effort, "summary": "detailed"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": request.prompt_cache_key,
        "text": {"verbosity": "low"},
        "client_metadata": {
            "session_id": request.prompt_cache_key,
            "thread_id": request.prompt_cache_key
        }
    }))
}

fn compaction_request(request: &ModelRequest<'_>) -> Result<Value> {
    let mut body = completion_request(request)?;
    body["input"]
        .as_array_mut()
        .context("Codex compaction input is not an array")?
        .push(json!({"type": "compaction_trigger"}));
    Ok(body)
}

fn response_from_item(item: &Value) -> Result<Option<ModelResponse>> {
    Ok(match item["type"].as_str() {
        Some("message") => {
            let content = item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|content| content["type"] == "output_text")
                .filter_map(|content| content["text"].as_str())
                .collect::<String>();
            if content.is_empty() {
                None
            } else {
                let phase = match item["phase"].as_str() {
                    Some("commentary") => Some(atra_protocol::AssistantMessagePhase::Commentary),
                    Some("final_answer") => Some(atra_protocol::AssistantMessagePhase::FinalAnswer),
                    _ => None,
                };
                Some(ModelResponse::AssistantMessage { content, phase })
            }
        }
        Some("function_call") => Some(ModelResponse::ToolCall {
            name: string_field(item, "name")?,
            arguments: serde_json::from_str(&string_field(item, "arguments")?)
                .context("Codex returned invalid tool arguments")?,
            call_id: item["call_id"].as_str().map(str::to_owned),
        }),
        Some("custom_tool_call") => Some(ModelResponse::CustomToolCall {
            item_id: item["id"].as_str().map(str::to_owned),
            name: string_field(item, "name")?,
            input: string_field(item, "input")?,
            call_id: string_field(item, "call_id")?,
        }),
        Some("web_search_call") => Some(ModelResponse::WebSearch { item: item.clone() }),
        Some("reasoning") => Some(ModelResponse::Reasoning { item: item.clone() }),
        _ => None,
    })
}

fn model_input(events: &[Event]) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    let message = |role: &str, text: String| json!({"type": "message", "role": role, "content": [{"type": "input_text", "text": text}]});
    if let Some(context) = events.iter().find_map(|event| match &event.data {
        ThreadEventData::ThreadContext(context) => Some(context),
        _ => None,
    }) {
        items.push(message("developer", context.content.clone()));
    }
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
            "stored compaction belongs to another provider"
        );
        items.extend(
            output
                .data
                .as_array()
                .context("stored compaction contains invalid Codex response items")?
                .iter()
                .cloned(),
        );
        &events[index + 1..]
    } else {
        events
    };
    let masked = crate::storage::latest_frozen_boundary(events)
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
                "stored model output belongs to another provider"
            );
            items.extend(
                output
                    .data
                    .as_array()
                    .context("stored model output contains invalid Codex response items")?
                    .iter()
                    .cloned(),
            );
            continue;
        }
        let item = match &event.data {
            ThreadEventData::ThreadContext(_) => None,
            ThreadEventData::WorkspaceInstructions(value) => Some(message(
                "developer",
                format!(
                    "# AGENTS.md instructions\n\n<INSTRUCTIONS>\n{}\n</INSTRUCTIONS>",
                    instruction_text(value, "AGENTS.md instructions")
                ),
            )),
            ThreadEventData::Skills(value) => {
                Some(message("developer", instruction_text(value, "skills list")))
            }
            ThreadEventData::Runners(value) => Some(message(
                "developer",
                match value {
                    RunnersEvent::Initial(runners) => format_runners(runners),
                    RunnersEvent::Replacement(runners) => format!(
                        "The available Atra Runner list has changed. This list replaces the previously provided list.\n\n{}",
                        format_runners(runners)
                    ),
                },
            )),
            ThreadEventData::UserMessage(value) => Some(message("user", value.content.clone())),
            ThreadEventData::ToolResult(ToolResultEvent::Custom { call_id, name, .. }) => {
                call_id.as_ref().map(|call_id| {
                    json!({
                        "type": "custom_tool_call_output",
                        "call_id": call_id,
                        "name": name,
                        "output": tool_result_text(projected_tool_result(event, &masked))
                    })
                })
            }
            ThreadEventData::ToolResult(ToolResultEvent::Function { call_id, .. }) => {
                call_id.as_ref().map(|call_id| {
                    json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": tool_result_text(projected_tool_result(event, &masked))
                    })
                })
            }
            _ => None,
        };
        if let Some(item) = item {
            items.push(item);
        }
    }
    Ok(items)
}

fn projected_tool_result<'a>(
    event: &'a Event,
    masked: &HashSet<atra_protocol::EventSequence>,
) -> &'a Value {
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
        _ => unreachable!(),
    };
    if masked.contains(&event.sequence) {
        masked_result.as_ref().unwrap_or(result)
    } else {
        result
    }
}

pub(super) fn context_tokens(events: &[Event]) -> Result<usize> {
    Ok(super::text_tokens(&serde_json::to_string(&model_input(
        events,
    )?)?))
}

fn tool_definitions(tools: &[ModelTool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| match tool {
            ModelTool::WebSearch => json!({"type": "web_search", "external_web_access": true}),
            ModelTool::Function {
                name,
                description,
                parameters,
            } => json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
                "strict": true,
            }),
            ModelTool::Custom { name, description, format } => json!({
                "type": "custom",
                "name": name,
                "description": description,
                "format": {"type": "grammar", "syntax": format.syntax, "definition": format.definition}
            }),
        })
        .collect()
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
    }
}

fn instruction_text(event: &InstructionEvent, label: &str) -> String {
    match event {
        InstructionEvent::Initial(content) => content.clone(),
        InstructionEvent::Replacement(content) => {
            format!("These {label} replace all previously provided {label}.\n\n{content}")
        }
        InstructionEvent::Removal => format!("The previously provided {label} no longer apply."),
    }
}

fn tool_result_text(result: &Value) -> String {
    result
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| result.to_string())
}

fn string_field(value: &Value, field: &str) -> Result<String> {
    value[field]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("Codex response item has no {field}"))
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

async fn decode_compaction_stream(response: Response) -> Result<Value> {
    if !response.status().is_success() {
        return Err(response_error(response, "Codex compaction").await);
    }
    let mut bytes = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut compaction = None;
    let mut compaction_count = 0;
    let mut output_count = 0;
    let mut completed = false;
    'stream: loop {
        let chunk = tokio::time::timeout(STREAM_IDLE_TIMEOUT, bytes.next())
            .await
            .context("Codex compaction response timed out")?;
        let Some(chunk) = chunk else {
            break;
        };
        buffer.extend_from_slice(&chunk.context("failed to read Codex compaction stream")?);
        while let Some(frame) = next_sse_frame(&mut buffer)? {
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                break 'stream;
            }
            let event: Value =
                serde_json::from_str(&data).context("invalid Codex compaction SSE event")?;
            match event["type"].as_str() {
                Some("response.output_item.done") => {
                    output_count += 1;
                    if event["item"]["type"] == "compaction" {
                        compaction_count += 1;
                        if compaction.is_none() {
                            compaction = Some(event["item"].clone());
                        }
                    }
                }
                Some("response.completed") => {
                    completed = true;
                    break 'stream;
                }
                Some("error" | "response.failed") => return Err(sse_failure(&event)),
                _ => {}
            }
        }
    }
    anyhow::ensure!(
        completed,
        "Codex compaction stream ended before response.completed"
    );
    anyhow::ensure!(
        compaction_count == 1,
        "Codex compaction expected exactly one compaction item, got {compaction_count} from {output_count} output items"
    );
    compaction.context("Codex compaction response has no compaction item")
}

async fn response_error(response: Response, label: &str) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow::anyhow!("{label} failed ({status}): {}", error_message(&body))
}

#[derive(Debug)]
struct RetryableStreamError(anyhow::Error);

impl std::fmt::Display for RetryableStreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for RetryableStreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::CONFLICT
            | StatusCode::TOO_EARLY
            | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}

fn is_retryable_stream_error(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source.is::<RetryableStreamError>()
            || source
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|error| error.is_timeout() || error.is_connect() || error.is_body())
    })
}

fn should_retry_stream(error: &anyhow::Error, emitted: bool, attempt: u64) -> bool {
    !emitted && attempt < STREAM_MAX_RETRIES && is_retryable_stream_error(error)
}

fn next_sse_frame(buffer: &mut Vec<u8>) -> Result<Option<String>> {
    let boundary = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n" || window == b"\r\r")
                .map(|index| (index, 2))
        });
    let Some((index, delimiter_len)) = boundary else {
        return Ok(None);
    };
    let frame = buffer.drain(..index).collect::<Vec<_>>();
    buffer.drain(..delimiter_len);
    String::from_utf8(frame)
        .context("Codex SSE event is not valid UTF-8")
        .map(Some)
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

fn merge_rate_limits(current: &mut Vec<Value>, updates: Vec<Value>) {
    for update in updates {
        let limit_id = update
            .get("limit_id")
            .and_then(Value::as_str)
            .unwrap_or("codex");
        if let Some(existing) = current.iter_mut().find(|snapshot| {
            snapshot
                .get("limit_id")
                .and_then(Value::as_str)
                .unwrap_or("codex")
                == limit_id
        }) {
            merge_json(existing, update);
        } else {
            current.push(update);
        }
    }
}

fn merge_json(current: &mut Value, update: Value) {
    match (current, update) {
        (Value::Object(current), Value::Object(update)) => {
            for (key, value) in update {
                if value.is_null() {
                    continue;
                }
                if let Some(current) = current.get_mut(&key) {
                    merge_json(current, value);
                } else {
                    current.insert(key, value);
                }
            }
        }
        (current, update) if !update.is_null() => *current = update,
        _ => {}
    }
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

fn sse_failure(event: &Value) -> anyhow::Error {
    let error = event
        .pointer("/response/error")
        .or_else(|| event.get("error"))
        .unwrap_or(event);
    let message = error
        .get("message")
        .or_else(|| event.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    let code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let status = error
        .get("status")
        .or_else(|| error.get("status_code"))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .and_then(|status| StatusCode::from_u16(status).ok());
    let retryable = status.is_some_and(retryable_status)
        || matches!(
            code,
            "server_error"
                | "internal_server_error"
                | "rate_limit_exceeded"
                | "overloaded"
                | "service_unavailable"
                | "timeout"
        );
    let error = anyhow::anyhow!("Codex response failed: {message}");
    if retryable {
        RetryableStreamError(error).into()
    } else {
        error
    }
}

fn backoff(attempt: u64) -> Duration {
    let base_ms = 200.0 * 2.0_f64.powi(attempt.saturating_sub(1) as i32);
    Duration::from_millis((base_ms * rand::rng().random_range(0.9..1.1)) as u64)
}

#[cfg(test)]
mod tests {
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
        let tools = crate::tools::model_tools(true);
        let request = completion_request(&ModelRequest {
            model: "model",
            reasoning_effort: "medium",
            instructions: super::super::BASE_INSTRUCTIONS,
            tools: &tools,
            events: &events,
            prompt_cache_key: "cache",
        })
        .unwrap();
        assert_eq!(request["model"], "model");
        assert_eq!(request["input"].as_array().unwrap().len(), 1);
        assert_eq!(
            request.pointer("/input/0/content/0/text"),
            Some(&json!("hello"))
        );
    }

    #[test]
    fn thread_context_survives_compaction() {
        let events = vec![
            Event {
                sequence: atra_protocol::EventSequence(0),
                data: ThreadEventData::ThreadContext(atra_protocol::MessageEvent {
                    content: "Thread context:\n- position: root (thread 1)".to_owned(),
                }),
            },
            Event {
                sequence: atra_protocol::EventSequence(1),
                data: ThreadEventData::Compaction(atra_protocol::CompactionEvent {
                    items: serde_json::to_value(ProviderOutput {
                        provider: PROVIDER_ID.to_owned(),
                        data: json!([]),
                    })
                    .unwrap(),
                    checkpoint_id: atra_protocol::CheckpointId(1),
                }),
            },
            Event {
                sequence: atra_protocol::EventSequence(2),
                data: ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                    content: "continue".to_owned(),
                }),
            },
        ];

        let input = model_input(&events).unwrap();
        assert_eq!(
            input[0].pointer("/content/0/text"),
            Some(&json!("Thread context:\n- position: root (thread 1)"))
        );
        assert_eq!(
            input[1].pointer("/content/0/text"),
            Some(&json!("continue"))
        );
    }

    #[test]
    fn compaction_request_uses_responses_v2_trigger() {
        let events = vec![Event {
            sequence: atra_protocol::EventSequence(0),
            data: ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                content: "hello".to_owned(),
            }),
        }];
        let tools = crate::tools::model_tools(true);
        let request = compaction_request(&ModelRequest {
            model: "model",
            reasoning_effort: "medium",
            instructions: super::super::BASE_INSTRUCTIONS,
            tools: &tools,
            events: &events,
            prompt_cache_key: "cache",
        })
        .unwrap();

        assert_eq!(
            request["input"].as_array().unwrap().last(),
            Some(&json!({"type": "compaction_trigger"}))
        );
        assert_eq!(request["stream"], true);
        assert_eq!(request["store"], false);
        assert_eq!(request["parallel_tool_calls"], true);
    }

    #[test]
    fn sse_frame_waits_for_complete_utf8_data() {
        let event = "data: {\"delta\":\"日本語\"}";
        let bytes = event.as_bytes();
        let split = bytes
            .windows("日".len())
            .position(|window| window == "日".as_bytes())
            .unwrap()
            + 1;
        let mut buffer = bytes[..split].to_vec();

        assert!(next_sse_frame(&mut buffer).unwrap().is_none());
        buffer.extend_from_slice(&bytes[split..]);
        buffer.extend_from_slice(b"\r\n\r\n");

        assert_eq!(next_sse_frame(&mut buffer).unwrap().as_deref(), Some(event));
        assert!(buffer.is_empty());
    }

    #[test]
    fn sse_frame_keeps_following_frames_buffered() {
        let mut buffer = b"data: one\n\ndata: two\n\n".to_vec();

        assert_eq!(
            next_sse_frame(&mut buffer).unwrap().as_deref(),
            Some("data: one")
        );
        assert_eq!(
            next_sse_frame(&mut buffer).unwrap().as_deref(),
            Some("data: two")
        );
        assert!(next_sse_frame(&mut buffer).unwrap().is_none());
    }

    #[test]
    fn stream_retry_requires_retryable_error_before_output() {
        let retryable = anyhow::Error::new(RetryableStreamError(anyhow::anyhow!("disconnected")));
        let permanent = anyhow::anyhow!("invalid SSE");

        assert!(should_retry_stream(&retryable, false, 0));
        assert!(!should_retry_stream(&retryable, true, 0));
        assert!(!should_retry_stream(&retryable, false, STREAM_MAX_RETRIES));
        assert!(!should_retry_stream(&permanent, false, 0));
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_status(StatusCode::BAD_GATEWAY));
        assert!(!retryable_status(StatusCode::BAD_REQUEST));
        assert!(!retryable_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn completed_response_requires_assistant_message_or_tool_call() {
        let error = ensure_completed_response(false).unwrap_err();

        assert!(error.to_string().contains("without an assistant message"));
        ensure_completed_response(true).unwrap();
    }

    #[test]
    fn raw_usage_windows_are_normalized_and_disabled_windows_stay_null() {
        let snapshots = rate_limit_snapshots(&json!({
            "rate_limit": {
                "primary_window": null,
                "secondary_window": {
                    "used_percent": 37.5,
                    "limit_window_seconds": 7 * 24 * 60 * 60,
                    "reset_at": 2_000_000_000
                }
            }
        }));

        assert_eq!(snapshots.len(), 1);
        assert!(snapshots[0]["primary"].is_null());
        assert_eq!(
            snapshots[0]["secondary"],
            json!({
                "used_percent": 37.5,
                "window_minutes": 7 * 24 * 60,
                "resets_at": 2_000_000_000
            })
        );
    }

    #[tokio::test]
    async fn rate_limits_after_completion_are_still_processed() {
        let (sender, mut receiver) = mpsc::channel(4);
        let mut response_id = None;
        let mut token_usage = None;
        let mut rate_limits = Vec::new();
        let mut emitted = false;
        let mut has_response = false;

        let completed = handle_sse_event(
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "id": "message-1",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}]
                }
            }),
            &sender,
            &mut response_id,
            &mut token_usage,
            &mut rate_limits,
            &mut emitted,
            &mut has_response,
        )
        .await
        .unwrap();
        assert!(!completed);
        assert!(has_response);
        assert!(receiver.recv().await.unwrap().is_ok());

        let completed = handle_sse_event(
            json!({"type": "response.completed", "response": {"id": "response-1"}}),
            &sender,
            &mut response_id,
            &mut token_usage,
            &mut rate_limits,
            &mut emitted,
            &mut has_response,
        )
        .await
        .unwrap();
        assert!(completed);

        let completed = handle_sse_event(
            json!({
                "type": "codex.rate_limits",
                "rate_limits": [{
                    "limit_id": "codex",
                    "primary": {"used_percent": 42}
                }]
            }),
            &sender,
            &mut response_id,
            &mut token_usage,
            &mut rate_limits,
            &mut emitted,
            &mut has_response,
        )
        .await
        .unwrap();
        assert!(!completed);
        assert_eq!(rate_limits[0]["primary"]["used_percent"], 42);
    }

    #[tokio::test]
    async fn reasoning_summary_part_boundary_is_streamed() {
        let (sender, mut receiver) = mpsc::channel(1);
        let mut response_id = None;
        let mut token_usage = None;
        let mut rate_limits = Vec::new();
        let mut emitted = false;
        let mut has_response = false;

        let completed = handle_sse_event(
            json!({"type": "response.reasoning_summary_part.added"}),
            &sender,
            &mut response_id,
            &mut token_usage,
            &mut rate_limits,
            &mut emitted,
            &mut has_response,
        )
        .await
        .unwrap();

        assert!(!completed);
        assert!(emitted);
        assert!(matches!(
            receiver.recv().await.unwrap().unwrap(),
            ModelEvent::Update(ModelStreamEvent::ReasoningSummaryPartAdded)
        ));
    }

    #[tokio::test]
    async fn completed_response_usage_is_normalized() {
        let (sender, _receiver) = mpsc::channel(1);
        let mut response_id = None;
        let mut token_usage = None;
        let mut rate_limits = Vec::new();
        let mut emitted = false;
        let mut has_response = false;

        let completed = handle_sse_event(
            json!({
                "type": "response.completed",
                "response": {
                    "id": "response-1",
                    "usage": {
                        "input_tokens": 6567,
                        "input_tokens_details": {
                            "cached_tokens": 3456,
                            "cache_write_tokens": 12
                        },
                        "output_tokens": 17,
                        "output_tokens_details": {"reasoning_tokens": 4},
                        "total_tokens": 6584
                    }
                }
            }),
            &sender,
            &mut response_id,
            &mut token_usage,
            &mut rate_limits,
            &mut emitted,
            &mut has_response,
        )
        .await
        .unwrap();

        assert!(completed);
        assert_eq!(
            token_usage,
            Some(json!({
                "input_tokens": 6567,
                "cached_input_tokens": 3456,
                "cache_write_input_tokens": 12,
                "output_tokens": 17,
                "reasoning_output_tokens": 4,
                "total_tokens": 6584
            }))
        );
    }

    #[tokio::test]
    async fn transient_sse_failure_is_retryable() {
        let (sender, _receiver) = mpsc::channel(1);
        let mut response_id = None;
        let mut token_usage = None;
        let mut rate_limits = Vec::new();
        let mut emitted = false;

        let error = handle_sse_event(
            json!({
                "type": "response.failed",
                "response": {
                    "error": {
                        "code": "server_error",
                        "message": "try again"
                    }
                }
            }),
            &sender,
            &mut response_id,
            &mut token_usage,
            &mut rate_limits,
            &mut emitted,
            &mut false,
        )
        .await
        .unwrap_err();

        assert!(error.downcast_ref::<RetryableStreamError>().is_some());
        assert!(should_retry_stream(&error, emitted, 0));
    }

    #[tokio::test]
    async fn invalid_request_sse_failure_is_not_retryable() {
        let (sender, _receiver) = mpsc::channel(1);
        let mut response_id = None;
        let mut token_usage = None;
        let mut rate_limits = Vec::new();
        let mut emitted = false;

        let error = handle_sse_event(
            json!({
                "type": "response.failed",
                "response": {
                    "error": {
                        "code": "invalid_request_error",
                        "message": "bad input"
                    }
                }
            }),
            &sender,
            &mut response_id,
            &mut token_usage,
            &mut rate_limits,
            &mut emitted,
            &mut false,
        )
        .await
        .unwrap_err();

        assert!(error.downcast_ref::<RetryableStreamError>().is_none());
        assert!(!should_retry_stream(&error, emitted, 0));
    }

    #[test]
    fn rate_limit_headers_include_all_series_and_preserve_cached_fields() {
        let mut headers = HeaderMap::new();
        headers.insert("x-codex-primary-used-percent", "12".parse().unwrap());
        headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        headers.insert("x-codex-primary-reset-at", "2000000000".parse().unwrap());
        headers.insert("x-codex-credits-balance", "4.5".parse().unwrap());
        headers.insert(
            "x-codex-research-primary-used-percent",
            "34".parse().unwrap(),
        );

        let updates = rate_limit_headers(&headers);
        assert_eq!(updates.len(), 2);
        assert_eq!(
            updates
                .iter()
                .find(|value| value["limit_id"] == "codex")
                .and_then(|value| value.pointer("/credits/balance")),
            Some(&json!(4.5))
        );
        assert_eq!(
            updates
                .iter()
                .find(|value| value["limit_id"] == "research")
                .and_then(|value| value.pointer("/primary/used_percent")),
            Some(&json!(34.0))
        );
        assert!(
            updates
                .iter()
                .all(|value| value.get("secondary").is_some_and(Value::is_null))
        );
        assert_eq!(
            updates
                .iter()
                .find(|value| value["limit_id"] == "codex")
                .and_then(|value| value.pointer("/primary/resets_at")),
            Some(&json!(2_000_000_000.0))
        );

        let mut cached = vec![json!({
            "limit_id": "codex",
            "primary": {"used_percent": 1.0},
            "credits": {"balance": 9.0},
            "plan_type": "pro"
        })];
        merge_rate_limits(
            &mut cached,
            vec![json!({
                "limit_id": "codex",
                "primary": {"used_percent": 12.0},
                "credits": null,
                "plan_type": null
            })],
        );
        assert_eq!(
            cached[0].pointer("/primary/used_percent"),
            Some(&json!(12.0))
        );
        assert_eq!(cached[0].pointer("/credits/balance"), Some(&json!(9.0)));
        assert_eq!(cached[0]["plan_type"], "pro");
    }

    #[tokio::test]
    async fn dropped_receiver_stops_sse_handling() {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let mut response_id = None;
        let mut token_usage = None;
        let mut rate_limits = Vec::new();
        let mut emitted = false;

        let error = handle_sse_event(
            json!({"type": "response.output_text.delta", "delta": "hello"}),
            &sender,
            &mut response_id,
            &mut token_usage,
            &mut rate_limits,
            &mut emitted,
            &mut false,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("receiver dropped"));
        assert!(emitted);
    }

    #[tokio::test]
    async fn completed_web_search_emits_final_action_before_output() {
        let (sender, mut receiver) = mpsc::channel(2);
        let mut response_id = None;
        let mut token_usage = None;
        let mut rate_limits = Vec::new();
        let mut emitted = false;

        handle_sse_event(
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "web_search_call",
                    "id": "search-1",
                    "status": "completed",
                    "action": {"type": "search", "query": "atra"}
                }
            }),
            &sender,
            &mut response_id,
            &mut token_usage,
            &mut rate_limits,
            &mut emitted,
            &mut false,
        )
        .await
        .unwrap();

        assert!(matches!(
            receiver.recv().await.unwrap().unwrap(),
            ModelEvent::Update(ModelStreamEvent::WebSearchUpdate { .. })
        ));
        assert!(matches!(
            receiver.recv().await.unwrap().unwrap(),
            ModelEvent::OutputItemDone { .. }
        ));
    }
}
