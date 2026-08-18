use std::{
    collections::{HashSet, VecDeque},
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::RwLock,
};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use atra_protocol::{
    AssistantMessagePhase, InstructionEvent, Model, RunnersEvent, ThreadEventData, ToolResultEvent,
};
use futures_util::{StreamExt, stream};
use rand::Rng;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ModelResponse, ModelSession,
    ModelStreamEvent, ModelTool, ProviderLoginStatus, ProviderOutput, format_runners,
};
use crate::storage::Event;

const PROVIDER_ID: &str = super::OLLAMA_PROVIDER;
const API_BASE: &str = "https://ollama.com/api";
const KEY_FILE: &str = "api-key";

pub(crate) struct OllamaProvider {
    auth_home: PathBuf,
    api_key: RwLock<Option<String>>,
    client: reqwest::Client,
    models: tokio::sync::RwLock<Option<Vec<Model>>>,
}

struct OllamaTurn<'a> {
    provider: &'a OllamaProvider,
    session_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Message {
    role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    content: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    thinking: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ToolCall {
    #[serde(default)]
    id: Option<String>,
    function: ToolFunction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ToolFunction {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Message,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<i64>,
    #[serde(default)]
    eval_count: Option<i64>,
    #[serde(default)]
    error: Option<String>,
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

struct OllamaStream {
    response: Response,
    buffer: Vec<u8>,
    message: Message,
    session_id: String,
    pending: VecDeque<Result<ModelEvent>>,
    finished: bool,
}

impl OllamaProvider {
    pub(super) fn new(auth_home: PathBuf) -> Self {
        let api_key = fs::read_to_string(auth_home.join(KEY_FILE))
            .ok()
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty());
        Self {
            auth_home,
            api_key: RwLock::new(api_key),
            client: reqwest::Client::new(),
            models: tokio::sync::RwLock::new(None),
        }
    }

    fn key(&self) -> Result<String> {
        self.api_key
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .context("Ollama login required; run `atra ollama login`")
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
            bail!("Ollama API key was rejected; run `atra ollama login`");
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

    async fn chat(&self, body: Value) -> Result<Response> {
        self.send(self.client.post(format!("{API_BASE}/chat")).json(&body))
            .await
    }

    fn store_key(&self, key: String) -> Result<()> {
        let key = key.trim();
        ensure!(!key.is_empty(), "Ollama API key must not be empty");
        fs::create_dir_all(&self.auth_home).with_context(|| {
            format!(
                "failed to create Ollama auth directory {}",
                self.auth_home.display()
            )
        })?;
        fs::set_permissions(&self.auth_home, fs::Permissions::from_mode(0o700))?;
        let path = self.auth_home.join(KEY_FILE);
        fs::write(&path, format!("{key}\n"))
            .with_context(|| format!("failed to save Ollama API key to {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        *self
            .api_key
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(key.to_owned());
        Ok(())
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
                reasoning_efforts(&tag.model, thinking);
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
            });
        }
        *self.models.write().await = Some(models.clone());
        Ok(models)
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    async fn models(&self) -> Result<Vec<Model>> {
        self.catalog().await
    }

    async fn login(&self, credential: Option<String>) -> Result<ProviderLoginStatus> {
        let credential = credential.context("Ollama login requires an API key")?;
        ensure!(
            !credential.trim().is_empty(),
            "Ollama API key must not be empty"
        );
        let previous = self
            .api_key
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(credential.trim().to_owned());
        *self.models.write().await = None;
        if let Err(error) = self.catalog().await {
            *self
                .api_key
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = previous;
            *self.models.write().await = None;
            return Err(error);
        }
        self.store_key(credential)?;
        Ok(ProviderLoginStatus::LoggedIn(None))
    }

    async fn login_status(&self) -> Result<ProviderLoginStatus> {
        Ok(if self.key().is_ok() {
            ProviderLoginStatus::LoggedIn(None)
        } else {
            ProviderLoginStatus::LoginRequired
        })
    }

    async fn reload_auth(&self) -> Result<()> {
        let key = fs::read_to_string(self.auth_home.join(KEY_FILE))
            .ok()
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty());
        *self
            .api_key
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = key;
        *self.models.write().await = None;
        Ok(())
    }

    async fn logout(&self) -> Result<()> {
        let path = self.auth_home.join(KEY_FILE);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to remove Ollama API key"),
        }
        *self
            .api_key
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self.models.write().await = None;
        Ok(())
    }

    async fn rate_limits(&self) -> Result<Value> {
        Ok(Value::Array(Vec::new()))
    }

    async fn execute_tool(&self, name: &str, arguments: &Value) -> Result<Option<Value>> {
        let endpoint = match name {
            "web_search" => "web_search",
            "web_fetch" => "web_fetch",
            _ => return Ok(None),
        };
        let result = self
            .send(
                self.client
                    .post(format!("{API_BASE}/{endpoint}"))
                    .json(arguments),
            )
            .await?
            .json()
            .await
            .with_context(|| format!("failed to decode Ollama {name} result"))?;
        Ok(Some(result))
    }

    async fn start_turn(&self, session_id: &str) -> Result<Box<dyn ModelSession + '_>> {
        self.key()?;
        Ok(Box::new(OllamaTurn {
            provider: self,
            session_id: session_id.to_owned(),
        }))
    }

    fn context_tokens(&self, events: &[Event]) -> Result<usize> {
        Ok(super::text_tokens(&serde_json::to_string(events)?))
    }
}

#[async_trait]
impl ModelSession for OllamaTurn<'_> {
    async fn stream(&self, request: &ModelRequest<'_>) -> Result<ModelEventStream> {
        let body = json!({
            "model": request.model,
            "messages": request_messages(request)?,
            "tools": tool_definitions(request.tools),
            "stream": true,
            "think": think_value(request.reasoning_effort),
        });
        let response = self.provider.chat(body).await?;
        let state = OllamaStream {
            response,
            buffer: Vec::new(),
            message: Message {
                role: "assistant".to_owned(),
                ..Message::default()
            },
            session_id: self.session_id.clone(),
            pending: VecDeque::new(),
            finished: false,
        };
        Ok(stream::unfold(state, |mut state| async move {
            state.next_event().await.map(|event| (event, state))
        })
        .boxed())
    }

    async fn compact(&self, request: &ModelRequest<'_>) -> Result<Option<ProviderOutput>> {
        let mut messages = request_messages(request)?;
        messages.push(Message {
            role: "user".to_owned(),
            content: "Summarize the conversation above for another coding assistant. Preserve decisions, constraints, file paths, code changes, tool results that still matter, and unfinished work. Return only the summary.".to_owned(),
            ..Message::default()
        });
        let response: ChatChunk = self
            .provider
            .chat(json!({
                "model": request.model,
                "messages": messages,
                "stream": false,
                "think": false,
            }))
            .await?
            .json()
            .await
            .context("failed to decode Ollama compaction response")?;
        ensure!(
            !response.message.content.trim().is_empty(),
            "Ollama returned an empty compaction"
        );
        let summary = Message {
            role: "system".to_owned(),
            content: format!(
                "Summary of the earlier conversation:\n\n{}",
                response.message.content
            ),
            ..Message::default()
        };
        Ok(Some(ProviderOutput {
            provider: PROVIDER_ID.to_owned(),
            data: serde_json::to_value(vec![summary])?,
        }))
    }
}

impl OllamaStream {
    async fn next_event(&mut self) -> Option<Result<ModelEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.finished {
                return None;
            }
            if let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line = self.buffer.drain(..=newline).collect::<Vec<_>>();
                let line = &line[..line.len().saturating_sub(1)];
                if line.is_empty() {
                    continue;
                }
                let chunk: ChatChunk = match serde_json::from_slice(line) {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        self.finished = true;
                        return Some(Err(error).context("failed to decode Ollama stream chunk"));
                    }
                };
                if let Some(error) = chunk.error {
                    self.finished = true;
                    return Some(Err(anyhow::anyhow!("Ollama stream failed: {error}")));
                }
                if !chunk.message.thinking.is_empty() {
                    self.message.thinking.push_str(&chunk.message.thinking);
                    self.pending.push_back(Ok(ModelEvent::Update(
                        ModelStreamEvent::ReasoningSummaryDelta(chunk.message.thinking),
                    )));
                }
                let started_tool_call =
                    self.message.tool_calls.is_empty() && !chunk.message.tool_calls.is_empty();
                self.message.tool_calls.extend(chunk.message.tool_calls);
                if !chunk.message.content.is_empty() {
                    let content = chunk.message.content;
                    self.message.content.push_str(&content);
                    self.pending.push_back(Ok(ModelEvent::Update(
                        ModelStreamEvent::AssistantDelta {
                            content,
                            phase: assistant_phase(&self.message),
                        },
                    )));
                } else if started_tool_call && !self.message.content.is_empty() {
                    self.pending.push_back(Ok(ModelEvent::Update(
                        ModelStreamEvent::AssistantDelta {
                            content: String::new(),
                            phase: AssistantMessagePhase::Commentary,
                        },
                    )));
                }
                if chunk.done {
                    self.finish(chunk.prompt_eval_count, chunk.eval_count);
                }
                continue;
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.buffer.extend_from_slice(&chunk),
                Ok(None) if self.buffer.is_empty() => {
                    self.finished = true;
                    return Some(Err(anyhow::anyhow!(
                        "Ollama stream ended before a completion marker"
                    )));
                }
                Ok(None) => self.buffer.push(b'\n'),
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error).context("failed to read Ollama stream"));
                }
            }
        }
    }

    fn finish(&mut self, input_tokens: Option<i64>, output_tokens: Option<i64>) {
        let message = self.message.clone();
        if !message.thinking.is_empty() {
            self.pending.push_back(Ok(ModelEvent::OutputItemDone {
                output: empty_output(),
                response: Some(ModelResponse::Reasoning {
                    item: json!({"summary": [{"text": message.thinking}]}),
                }),
            }));
        }
        let stored = ProviderOutput {
            provider: PROVIDER_ID.to_owned(),
            data: serde_json::to_value(vec![message.clone()]).expect("message serializes"),
        };
        if !message.content.is_empty() {
            let phase = assistant_phase(&message);
            self.pending.push_back(Ok(ModelEvent::OutputItemDone {
                output: stored,
                response: Some(ModelResponse::AssistantMessage {
                    content: message.content,
                    phase,
                }),
            }));
        } else {
            self.pending.push_back(Ok(ModelEvent::OutputItemDone {
                output: stored,
                response: None,
            }));
        }
        for (index, call) in message.tool_calls.into_iter().enumerate() {
            let call_id = call.id.unwrap_or_else(|| {
                format!(
                    "ollama-{}-{index}",
                    rand::rng().random::<u64>() ^ super::text_tokens(&self.session_id) as u64
                )
            });
            let response = if call.function.name == "command" {
                custom_tool_response(call.function.name, call.function.arguments, call_id)
            } else {
                ModelResponse::ToolCall {
                    name: call.function.name,
                    arguments: call.function.arguments,
                    call_id: Some(call_id),
                }
            };
            self.pending.push_back(Ok(ModelEvent::OutputItemDone {
                output: empty_output(),
                response: Some(response),
            }));
        }
        let input_tokens = input_tokens.unwrap_or(0);
        let output_tokens = output_tokens.unwrap_or(0);
        self.pending.push_back(Ok(ModelEvent::Completed {
            metadata: None,
            token_usage: Some(json!({
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": input_tokens.saturating_add(output_tokens),
            })),
            rate_limits: Vec::new(),
        }));
        self.finished = true;
    }
}

fn assistant_phase(message: &Message) -> AssistantMessagePhase {
    if message.tool_calls.is_empty() {
        AssistantMessagePhase::FinalAnswer
    } else {
        AssistantMessagePhase::Commentary
    }
}

fn empty_output() -> ProviderOutput {
    ProviderOutput {
        provider: PROVIDER_ID.to_owned(),
        data: Value::Array(Vec::new()),
    }
}

fn think_value(effort: &str) -> Value {
    match effort {
        "low" | "medium" | "high" | "max" => Value::String(effort.to_owned()),
        "enabled" => Value::Bool(true),
        "disabled" | "none" => Value::Bool(false),
        _ => Value::Bool(true),
    }
}

fn reasoning_efforts(model: &str, thinking: bool) -> (String, Vec<String>) {
    if !thinking {
        return ("none".to_owned(), vec!["none".to_owned()]);
    }

    let model = model.to_ascii_lowercase();
    let (default, supported): (&str, &[&str]) = if model.starts_with("gpt-oss:") {
        ("medium", &["low", "medium", "high"])
    } else if model.starts_with("qwen3-vl")
        || model.starts_with("kimi-k2-thinking")
        || model.starts_with("minimax")
    {
        ("medium", &["low", "medium", "high", "max"])
    } else if model.starts_with("qwen3") {
        ("enabled", &["disabled", "enabled"])
    } else if model.starts_with("glm-5.2") {
        ("high", &["disabled", "high", "max"])
    } else {
        ("medium", &["disabled", "low", "medium", "high", "max"])
    };
    (
        default.to_owned(),
        supported
            .iter()
            .map(|effort| (*effort).to_owned())
            .collect(),
    )
}

fn tool_definitions(tools: &[ModelTool]) -> Vec<Value> {
    let mut definitions = Vec::new();
    for tool in tools {
        match tool {
            ModelTool::WebSearch => {
                definitions.push(function_tool(
                    "web_search",
                    "Search the web for current information.",
                    crate::tools::web_search_parameters(),
                ));
                definitions.push(function_tool(
                    "web_fetch",
                    "Fetch the readable content of a web page.",
                    crate::tools::web_fetch_parameters(),
                ));
            }
            ModelTool::Function {
                name,
                description,
                parameters,
            } => definitions.push(function_tool(name, description, parameters.clone())),
            ModelTool::Custom {
                name,
                description,
                format,
            } => {
                let description = format!(
                    "{description}\n\nThe complete input must conform to this {} grammar:\n\n{}",
                    format.syntax, format.definition
                );
                definitions.push(function_tool(
                    name,
                    &description,
                    crate::tools::custom_tool_wrapper_parameters(),
                ));
            }
        }
    }
    definitions
}

fn custom_tool_response(name: String, arguments: Value, call_id: String) -> ModelResponse {
    ModelResponse::CustomToolCall {
        item_id: None,
        name,
        input: super::CustomToolInput::Arguments(arguments),
        call_id,
    }
}

fn function_tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}

fn request_messages(request: &ModelRequest<'_>) -> Result<Vec<Message>> {
    let mut messages = vec![Message {
        role: "system".to_owned(),
        content: request.instructions.to_owned(),
        ..Message::default()
    }];
    messages.extend(event_messages(request.events)?);
    Ok(messages)
}

fn event_messages(events: &[Event]) -> Result<Vec<Message>> {
    let mut messages = Vec::new();
    if let Some(context) = events.iter().find_map(|event| match &event.data {
        ThreadEventData::ThreadContext(context) => Some(context),
        _ => None,
    }) {
        messages.push(Message {
            role: "system".to_owned(),
            content: context.content.clone(),
            ..Message::default()
        });
    }
    let events = if let Some(index) = events
        .iter()
        .rposition(|event| matches!(event.data, ThreadEventData::Compaction(_)))
    {
        let output = match &events[index].data {
            ThreadEventData::Compaction(compaction) => {
                serde_json::from_value::<ProviderOutput>(compaction.items.clone())?
            }
            _ => unreachable!(),
        };
        ensure!(
            output.provider == PROVIDER_ID,
            "stored compaction belongs to {}",
            output.provider
        );
        messages.extend(serde_json::from_value::<Vec<Message>>(output.data)?);
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
        if let ThreadEventData::ModelOutput(output) = &event.data {
            let output = serde_json::from_value::<ProviderOutput>(output.output.clone())?;
            ensure!(
                output.provider == PROVIDER_ID,
                "stored model output belongs to {}",
                output.provider
            );
            messages.extend(serde_json::from_value::<Vec<Message>>(output.data)?);
            continue;
        }
        let message = match &event.data {
            ThreadEventData::ThreadContext(_) => continue,
            ThreadEventData::WorkspaceInstructions(event) => system_instruction("AGENTS.md", event),
            ThreadEventData::Skills(event) => system_instruction("Skills", event),
            ThreadEventData::SkillInvocation(event) => Message {
                role: "user".to_owned(),
                content: super::format_skill_invocation(event),
                ..Message::default()
            },
            ThreadEventData::Runners(event) => Message {
                role: "system".to_owned(),
                content: match event {
                    RunnersEvent::Initial(runners) | RunnersEvent::Replacement(runners) => {
                        format_runners(runners)
                    }
                },
                ..Message::default()
            },
            ThreadEventData::UserMessage(event) => Message {
                role: "user".to_owned(),
                content: event.content.clone(),
                ..Message::default()
            },
            ThreadEventData::ToolResult(result) => {
                let (name, value, masked_value) = match result {
                    ToolResultEvent::Custom {
                        name,
                        result,
                        masked_result,
                        ..
                    }
                    | ToolResultEvent::Function {
                        name,
                        result,
                        masked_result,
                        ..
                    } => (name, result, masked_result),
                };
                let value = if masked.contains(&event.sequence) {
                    masked_value.as_ref().unwrap_or(value)
                } else {
                    value
                };
                Message {
                    role: "tool".to_owned(),
                    content: value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string()),
                    tool_name: Some(name.clone()),
                    ..Message::default()
                }
            }
            ThreadEventData::AssistantMessage(_)
            | ThreadEventData::WebSearch(_)
            | ThreadEventData::ToolCall(_)
            | ThreadEventData::FrozenBoundary(_)
            | ThreadEventData::Reasoning(_)
            | ThreadEventData::ModelOutput(_)
            | ThreadEventData::Compaction(_)
            | ThreadEventData::ModelRequest(_)
            | ThreadEventData::TokenUsage(_)
            | ThreadEventData::RateLimits(_) => continue,
            ThreadEventData::ApprovalDecision(_)
            | ThreadEventData::Retry(_)
            | ThreadEventData::TurnOutcome(_) => continue,
        };
        messages.push(message);
    }
    Ok(messages)
}

fn system_instruction(label: &str, event: &InstructionEvent) -> Message {
    Message {
        role: "system".to_owned(),
        content: match event {
            InstructionEvent::Initial(content) => format!("{label}:\n\n{content}"),
            InstructionEvent::Replacement(content) => {
                format!("The {label} instructions were replaced by:\n\n{content}")
            }
            InstructionEvent::Removal => format!("The previous {label} instructions were removed."),
        },
        ..Message::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atra_protocol::{EventSequence, ModelOutputEvent, ToolArtifact};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn ollama_tools_include_question_command_search_and_fetch() {
        let tools = crate::tools::model_tools(true);
        let definitions = tool_definitions(&tools);
        let names = definitions
            .iter()
            .filter_map(|tool| tool.pointer("/function/name")?.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["web_search", "web_fetch", "question", "command"]);
        let command_description = definitions[3]["function"]["description"].as_str().unwrap();
        assert!(command_description.contains("lark grammar"));
        assert!(command_description.contains("add_line: \"+\""));
        assert_eq!(
            definitions[0]["function"]["parameters"],
            crate::tools::web_search_parameters()
        );
        assert_eq!(
            definitions[1]["function"]["parameters"],
            crate::tools::web_fetch_parameters()
        );
        assert_eq!(
            definitions[3]["function"]["parameters"],
            crate::tools::custom_tool_wrapper_parameters()
        );
    }

    #[test]
    fn custom_tool_arguments_are_preserved_for_controller_validation() {
        let arguments = json!({
            "command": "echo wrong field",
            "description": "Run a command"
        });
        let response =
            custom_tool_response("command".to_owned(), arguments.clone(), "call-1".to_owned());

        let ModelResponse::CustomToolCall {
            name,
            input: crate::model::CustomToolInput::Arguments(input),
            call_id,
            ..
        } = response
        else {
            panic!("expected raw custom tool arguments");
        };
        assert_eq!(name, "command");
        assert_eq!(input, arguments);
        assert_eq!(call_id, "call-1");
    }

    #[test]
    fn thinking_efforts_map_to_ollama_values() {
        assert_eq!(think_value("medium"), json!("medium"));
        assert_eq!(think_value("max"), json!("max"));
        assert_eq!(think_value("enabled"), json!(true));
        assert_eq!(think_value("none"), json!(false));
    }

    #[test]
    fn reasoning_efforts_are_selected_for_model_families() {
        assert_eq!(
            reasoning_efforts("gpt-oss:120b-cloud", true),
            efforts("medium", &["low", "medium", "high"])
        );
        assert_eq!(
            reasoning_efforts("qwen3:235b-cloud", true),
            efforts("enabled", &["disabled", "enabled"])
        );
        assert_eq!(
            reasoning_efforts("qwen3-vl:235b-cloud", true),
            efforts("medium", &["low", "medium", "high", "max"])
        );
        assert_eq!(
            reasoning_efforts("glm-5.2:cloud", true),
            efforts("high", &["disabled", "high", "max"])
        );
        assert_eq!(
            reasoning_efforts("kimi-k2-thinking:cloud", true),
            efforts("medium", &["low", "medium", "high", "max"])
        );
        assert_eq!(
            reasoning_efforts("minimax-m2.1:cloud", true),
            efforts("medium", &["low", "medium", "high", "max"])
        );
        assert_eq!(
            reasoning_efforts("future-thinking-model:cloud", true),
            efforts("medium", &["disabled", "low", "medium", "high", "max"])
        );
        assert_eq!(
            reasoning_efforts("qwen3:235b-cloud", false),
            efforts("none", &["none"])
        );
    }

    fn efforts(default: &str, supported: &[&str]) -> (String, Vec<String>) {
        (
            default.to_owned(),
            supported
                .iter()
                .map(|effort| (*effort).to_owned())
                .collect(),
        )
    }

    #[test]
    fn assistant_content_is_commentary_before_tools_and_final_without_tools() {
        let mut message = Message {
            content: "working".to_owned(),
            ..Message::default()
        };
        assert_eq!(
            assistant_phase(&message),
            AssistantMessagePhase::FinalAnswer
        );

        message.tool_calls.push(ToolCall {
            id: Some("call".to_owned()),
            function: ToolFunction {
                name: "command".to_owned(),
                arguments: json!({"input": "echo"}),
            },
        });
        assert_eq!(assistant_phase(&message), AssistantMessagePhase::Commentary);
    }

    #[test]
    fn stored_outputs_and_tool_results_rebuild_ollama_messages() {
        let assistant = Message {
            role: "assistant".to_owned(),
            content: "checking".to_owned(),
            tool_calls: vec![ToolCall {
                id: None,
                function: ToolFunction {
                    name: "web_search".to_owned(),
                    arguments: json!({"query": "atra"}),
                },
            }],
            ..Message::default()
        };
        let events = vec![
            Event {
                sequence: EventSequence(0),
                data: ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                    content: "search".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(1),
                data: ThreadEventData::ModelOutput(ModelOutputEvent {
                    request_sequence: EventSequence(0),
                    output: serde_json::to_value(ProviderOutput {
                        provider: PROVIDER_ID.to_owned(),
                        data: serde_json::to_value(vec![assistant]).unwrap(),
                    })
                    .unwrap(),
                    response_id: None,
                }),
            },
            Event {
                sequence: EventSequence(2),
                data: ThreadEventData::ToolResult(ToolResultEvent::Function {
                    call_type: None,
                    name: "web_search".to_owned(),
                    call_id: Some("call".to_owned()),
                    result: json!({"results": []}),
                    artifacts: Vec::<ToolArtifact>::new(),
                    masked_result: None,
                }),
            },
        ];

        let messages = event_messages(&events).unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].tool_calls[0].function.name, "web_search");
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].tool_name.as_deref(), Some("web_search"));
    }

    #[test]
    fn thread_context_survives_compaction() {
        let events = vec![
            Event {
                sequence: EventSequence(0),
                data: ThreadEventData::ThreadContext(atra_protocol::MessageEvent {
                    content: "Thread context:\n- position: root (thread 1)".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(1),
                data: ThreadEventData::Compaction(atra_protocol::CompactionEvent {
                    items: serde_json::to_value(ProviderOutput {
                        provider: PROVIDER_ID.to_owned(),
                        data: serde_json::to_value(Vec::<Message>::new()).unwrap(),
                    })
                    .unwrap(),
                    checkpoint_id: atra_protocol::CheckpointId(1),
                }),
            },
            Event {
                sequence: EventSequence(2),
                data: ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                    content: "continue".to_owned(),
                }),
            },
        ];

        let messages = event_messages(&events).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(
            messages[0].content,
            "Thread context:\n- position: root (thread 1)"
        );
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "continue");
    }

    #[tokio::test]
    async fn credentials_are_private_and_reloadable() {
        let directory = tempfile::tempdir().unwrap();
        let auth_home = directory.path().join("ollama");
        let provider = OllamaProvider::new(auth_home.clone());
        provider.store_key("secret".to_owned()).unwrap();

        assert_eq!(
            fs::metadata(&auth_home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(auth_home.join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let reloaded = OllamaProvider::new(auth_home);
        assert!(matches!(
            ModelProvider::login_status(&reloaded).await.unwrap(),
            ProviderLoginStatus::LoggedIn(None)
        ));
    }
}
