use std::collections::VecDeque;

use anyhow::{Context, Result};
use atra_protocol::AssistantMessagePhase;
use futures_util::{StreamExt, stream};
use rand::Rng;
use reqwest::Response;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::{
    ModelEvent, ModelEventStream, ModelRequest, ModelResponse, ModelStreamEvent, ModelTool,
};
use crate::model::ollama::OllamaProvider;
use crate::storage::Event;

const PROVIDER_ID: &str = super::super::OLLAMA_PROVIDER;

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

struct OllamaStream {
    response: Response,
    buffer: Vec<u8>,
    message: Message,
    session_id: String,
    model: String,
    pending: VecDeque<Result<ModelEvent>>,
    finished: bool,
}

impl OllamaTurn<'_> {
    async fn stream(&self, request: &ModelRequest<'_>) -> Result<ModelEventStream> {
        let body = request_body(request, request_messages(request)?);
        let response = self.provider.chat(body).await?;
        let state = OllamaStream {
            response,
            buffer: Vec::new(),
            message: Message {
                role: "assistant".to_owned(),
                ..Message::default()
            },
            session_id: self.session_id.clone(),
            model: request.model.to_owned(),
            pending: VecDeque::new(),
            finished: false,
        };
        Ok(stream::unfold(state, |mut state| async move {
            state.next_event().await.map(|event| (event, state))
        })
        .boxed())
    }
}

fn request_body(request: &ModelRequest<'_>, messages: Vec<Message>) -> Value {
    json!({
        "model": request.model,
        "messages": messages,
        "tools": tool_definitions(request.tools),
        "stream": true,
        "think": think_value(request.reasoning_effort),
    })
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
                self.message.tool_calls.extend(chunk.message.tool_calls);
                if !chunk.message.content.is_empty() {
                    let content = chunk.message.content;
                    self.message.content.push_str(&content);
                    self.pending
                        .push_back(Ok(ModelEvent::Update(assistant_delta(content))));
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
                response: Some(ModelResponse::Reasoning {
                    summary: message.thinking.clone(),
                    opaque: atra_protocol::OpaqueState {
                        replay_key: format!("{PROVIDER_ID}/{}/thinking-v1", self.model),
                        payload: json!({"thinking": message.thinking}),
                    },
                }),
            }));
        }
        if !message.content.is_empty() {
            let phase = completed_assistant_phase(&message);
            self.pending.push_back(Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage {
                    content: message.content,
                    phase,
                }),
            }));
        }
        for (index, call) in message.tool_calls.into_iter().enumerate() {
            let call_id = call.id.unwrap_or_else(|| {
                format!(
                    "ollama-{}-{index}",
                    rand::rng().random::<u64>()
                        ^ super::super::text_tokens(&self.session_id) as u64
                )
            });
            let response = ModelResponse::ToolCall {
                name: call.function.name,
                arguments: call.function.arguments,
                call_id,
            };
            self.pending.push_back(Ok(ModelEvent::OutputItemDone {
                response: Some(response),
            }));
        }
        let input_tokens = input_tokens.unwrap_or(0);
        let output_tokens = output_tokens.unwrap_or(0);
        self.pending.push_back(Ok(ModelEvent::Completed {
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

fn assistant_delta(content: String) -> ModelStreamEvent {
    ModelStreamEvent::AssistantDelta {
        content,
        phase: AssistantMessagePhase::Commentary,
    }
}

fn completed_assistant_phase(message: &Message) -> AssistantMessagePhase {
    if message.tool_calls.is_empty() {
        AssistantMessagePhase::FinalAnswer
    } else {
        AssistantMessagePhase::Commentary
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

pub(crate) fn reasoning_efforts(model: &str, thinking: bool) -> (String, Vec<String>) {
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
            ModelTool::Tool { name, json, .. } => {
                let json_interface = json
                    .as_ref()
                    .expect("model tool must expose an Ollama-compatible interface");
                definitions.push(function_tool(
                    name,
                    &json_interface.description,
                    json_interface.parameters.clone(),
                ));
            }
        }
    }
    definitions
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

pub(crate) fn context_tokens(events: &[Event]) -> Result<usize> {
    Ok(super::super::text_tokens(&serde_json::to_string(
        &event_messages(events)?,
    )?))
}

fn event_messages(events: &[Event]) -> Result<Vec<Message>> {
    use super::super::surface::{Item, Role, ToolInput};

    let mut messages = Vec::new();
    for item in super::super::surface::derive(events, None)?.items {
        match item {
            Item::Message { role, text, .. } => messages.push(Message {
                role: match role {
                    Role::Developer => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                }
                .to_owned(),
                content: text,
                ..Message::default()
            }),
            Item::ToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                let call = ToolCall {
                    id: Some(call_id),
                    function: ToolFunction {
                        name,
                        arguments: match input {
                            ToolInput::Json(value) => value,
                            ToolInput::Text(value) => Value::String(value),
                        },
                    },
                };
                if let Some(last) = messages.last_mut()
                    && last.role == "assistant"
                    && last.content.is_empty()
                    && !last.tool_calls.is_empty()
                {
                    last.tool_calls.push(call);
                } else {
                    messages.push(Message {
                        role: "assistant".to_owned(),
                        tool_calls: vec![call],
                        ..Message::default()
                    });
                }
            }
            Item::ToolResult { name, output, .. } => messages.push(Message {
                role: "tool".to_owned(),
                content: output
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| output.to_string()),
                tool_name: Some(name),
                ..Message::default()
            }),
            Item::Reasoning { .. } | Item::WebSearch(_) | Item::Opaque(_) => {}
        }
    }
    Ok(messages)
}

pub(crate) async fn stream(
    provider: &OllamaProvider,
    session_id: &str,
    request: &ModelRequest<'_>,
) -> Result<ModelEventStream> {
    OllamaTurn {
        provider,
        session_id: session_id.to_owned(),
    }
    .stream(request)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use atra_protocol::{
        EventSequence, FrozenBoundaryEvent, ThreadEventData, ToolArtifact, ToolResultEvent,
    };

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
    fn context_tokens_use_masked_tool_results() {
        let events = vec![
            Event {
                sequence: EventSequence(0),
                data: ThreadEventData::ToolResult(ToolResultEvent::Function {
                    name: "command".to_owned(),
                    call_id: "call".to_owned(),
                    result: Value::String("large output ".repeat(10_000)),
                    artifacts: Vec::<ToolArtifact>::new(),
                    masked_result: Some(Value::String("output masked".to_owned())),
                }),
            },
            Event {
                sequence: EventSequence(1),
                data: ThreadEventData::FrozenBoundary(FrozenBoundaryEvent {
                    through_sequence: EventSequence(0),
                    masked_sequences: vec![EventSequence(0)],
                }),
            },
        ];

        let full_tokens = context_tokens(&events[..1]).unwrap();
        let masked_tokens = context_tokens(&events).unwrap();

        assert!(masked_tokens < full_tokens);
    }
}
