use std::collections::{HashSet, VecDeque};

use anyhow::{Context, Result, bail, ensure};
use atra_protocol::AssistantMessagePhase;
use futures_util::{StreamExt, stream};
use rand::Rng;
use reqwest::Response;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::{
    ModelEvent, ModelEventStream, ModelRequest, ModelResponse, ModelStreamEvent,
    surface::{Item, Role, ToolInput},
};
use crate::model::ollama::OllamaProvider;
use crate::storage::Event;

const PROVIDER_ID: &str = super::super::OLLAMA_PROVIDER;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct Message {
    #[serde(default)]
    role: String,
    #[serde(default)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    function: ToolFunction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ToolFunction {
    name: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
}

fn empty_arguments() -> Value {
    json!({})
}

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

struct NdjsonDecoder {
    response: Response,
    buffer: Vec<u8>,
    ended: bool,
}

#[derive(Default)]
struct Completion {
    thinking: String,
    content: String,
    tool_calls: Vec<ToolCall>,
}

struct OllamaStream {
    decoder: NdjsonDecoder,
    completion: Completion,
    session_id: String,
    model: String,
    pending: VecDeque<Result<ModelEvent>>,
    finished: bool,
}

pub(crate) async fn stream(
    provider: &OllamaProvider,
    session_id: &str,
    request: &ModelRequest<'_>,
) -> Result<ModelEventStream> {
    let response = provider.chat(request_body(request)?).await?;
    Ok(response_stream(response, session_id, request.model))
}

fn response_stream(response: Response, session_id: &str, model: &str) -> ModelEventStream {
    let state = OllamaStream {
        decoder: NdjsonDecoder::new(response),
        completion: Completion::default(),
        session_id: session_id.to_owned(),
        model: model.to_owned(),
        pending: VecDeque::new(),
        finished: false,
    };
    stream::unfold(state, |mut state| async move {
        state.next_event().await.map(|event| (event, state))
    })
    .boxed()
}

fn request_body(request: &ModelRequest<'_>) -> Result<Value> {
    let mut body = json!({
        "model": request.model,
        "messages": request_messages(request)?,
        "stream": true,
        "think": think_value(request.reasoning_effort)?,
    });
    let tools = super::function_tools(request.tools);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    Ok(body)
}

impl NdjsonDecoder {
    fn new(response: Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            ended: false,
        }
    }

    async fn next(&mut self) -> Result<Option<ChatChunk>> {
        loop {
            if let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line = self.buffer.drain(..=newline).collect::<Vec<_>>();
                if let Some(chunk) = decode_line(&line[..line.len() - 1])? {
                    return Ok(Some(chunk));
                }
                continue;
            }
            if self.ended {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.buffer);
                return decode_line(&line);
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.buffer.extend_from_slice(&chunk),
                Ok(None) => self.ended = true,
                Err(error) => {
                    return Err(error).context("failed to read Ollama stream");
                }
            }
        }
    }
}

fn decode_line(line: &[u8]) -> Result<Option<ChatChunk>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(line)
        .map(Some)
        .context("failed to decode Ollama stream chunk")
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
            let chunk = match self.decoder.next().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => {
                    self.finished = true;
                    return Some(Err(anyhow::anyhow!(
                        "Ollama stream ended before a completion marker"
                    )));
                }
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            };
            if let Err(error) = self.accept(chunk) {
                self.pending.clear();
                self.finished = true;
                return Some(Err(error));
            }
        }
    }

    fn accept(&mut self, chunk: ChatChunk) -> Result<()> {
        if let Some(error) = chunk.error {
            bail!("Ollama stream failed: {error}");
        }
        ensure!(!self.finished, "Ollama returned data after completion");
        if let Some(message) = chunk.message {
            if !message.thinking.is_empty() {
                self.completion.thinking.push_str(&message.thinking);
                self.pending.push_back(Ok(ModelEvent::Update(
                    ModelStreamEvent::ReasoningSummaryDelta(message.thinking),
                )));
            }
            if !message.content.is_empty() {
                self.completion.content.push_str(&message.content);
                self.pending
                    .push_back(Ok(ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                        content: message.content,
                        phase: AssistantMessagePhase::Commentary,
                    })));
            }
            self.completion.tool_calls.extend(message.tool_calls);
        }
        if chunk.done {
            let events = self.completion.finish(
                &self.session_id,
                &self.model,
                chunk.prompt_eval_count,
                chunk.eval_count,
            )?;
            self.pending.extend(events.into_iter().map(Ok));
            self.finished = true;
        }
        Ok(())
    }
}

impl Completion {
    fn finish(
        &self,
        session_id: &str,
        model: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) -> Result<Vec<ModelEvent>> {
        ensure!(
            !self.content.is_empty() || !self.tool_calls.is_empty(),
            "Ollama completed without assistant content or tool calls"
        );
        let mut events = Vec::new();
        if !self.thinking.is_empty() {
            events.push(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::Reasoning {
                    summary: self.thinking.clone(),
                    opaque: atra_protocol::OpaqueState {
                        replay_key: thinking_replay_key(model),
                        payload: json!({"thinking": self.thinking}),
                    },
                }),
            });
        }
        if !self.content.is_empty() {
            events.push(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage {
                    content: self.content.clone(),
                    phase: if self.tool_calls.is_empty() {
                        AssistantMessagePhase::FinalAnswer
                    } else {
                        AssistantMessagePhase::Commentary
                    },
                }),
            });
        }

        let mut call_ids = HashSet::new();
        for (index, call) in self.tool_calls.iter().enumerate() {
            ensure!(
                !call.function.name.is_empty(),
                "Ollama tool call omitted function name"
            );
            ensure!(
                call.function.arguments.is_object(),
                "Ollama tool call {} returned non-object arguments",
                call.function.name
            );
            let call_id = match call.id.as_deref() {
                Some("") => bail!("Ollama tool call returned an empty id"),
                Some(call_id) => call_id.to_owned(),
                None => fallback_call_id(session_id, index),
            };
            ensure!(
                call_ids.insert(call_id.clone()),
                "Ollama returned duplicate tool call id {call_id}"
            );
            events.push(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::ToolCall {
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                    call_id,
                }),
            });
        }
        events.push(ModelEvent::Completed {
            token_usage: token_usage(input_tokens, output_tokens),
            rate_limits: Vec::new(),
        });
        Ok(events)
    }
}

fn token_usage(input_tokens: Option<u64>, output_tokens: Option<u64>) -> Option<Value> {
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": input_tokens
            .zip(output_tokens)
            .map(|(input, output)| input.saturating_add(output)),
    }))
}

fn fallback_call_id(session_id: &str, index: usize) -> String {
    format!(
        "ollama-{}-{index}",
        rand::rng().random::<u64>() ^ super::super::text_tokens(session_id) as u64
    )
}

fn thinking_replay_key(model: &str) -> String {
    format!("{PROVIDER_ID}/{model}/thinking-v1")
}

fn think_value(effort: &str) -> Result<Value> {
    Ok(match effort {
        "low" | "medium" | "high" | "max" => Value::String(effort.to_owned()),
        "enabled" => Value::Bool(true),
        "disabled" | "none" => Value::Bool(false),
        other => bail!("unsupported Ollama reasoning option {other}"),
    })
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

fn request_messages(request: &ModelRequest<'_>) -> Result<Vec<Message>> {
    let mut messages = vec![Message {
        role: "system".to_owned(),
        content: request.instructions.to_owned(),
        ..Message::default()
    }];
    messages.extend(event_messages(
        request.events,
        Some(&thinking_replay_key(request.model)),
    )?);
    Ok(messages)
}

pub(crate) fn context_tokens(events: &[Event]) -> Result<usize> {
    Ok(super::super::text_tokens(&serde_json::to_string(
        &event_messages(events, None)?,
    )?))
}

fn event_messages(events: &[Event], replay_key: Option<&str>) -> Result<Vec<Message>> {
    let mut messages = Vec::new();
    for item in super::super::surface::derive(events, None)?.items {
        match item {
            Item::Message { role, text, .. } => match role {
                Role::Developer => messages.push(Message {
                    role: "system".to_owned(),
                    content: text,
                    ..Message::default()
                }),
                Role::User => messages.push(Message {
                    role: "user".to_owned(),
                    content: text,
                    ..Message::default()
                }),
                Role::Assistant => assistant_message(&mut messages).content.push_str(&text),
            },
            Item::Reasoning { summary, opaque } => {
                let thinking = match replay_key {
                    Some(replay_key) => opaque
                        .filter(|opaque| opaque.replay_key == replay_key)
                        .map(|opaque| {
                            opaque.payload["thinking"]
                                .as_str()
                                .context("Ollama opaque reasoning omitted string field thinking")
                                .map(str::to_owned)
                        })
                        .transpose()?,
                    None => (!summary.is_empty()).then_some(summary),
                };
                if let Some(thinking) = thinking {
                    assistant_message(&mut messages)
                        .thinking
                        .push_str(&thinking);
                }
            }
            Item::ToolCall {
                call_id,
                name,
                input,
                ..
            } => assistant_message(&mut messages).tool_calls.push(ToolCall {
                id: Some(call_id),
                function: ToolFunction {
                    name,
                    arguments: match input {
                        ToolInput::Json(value) => value,
                        ToolInput::Text(value) => json!({"input": value}),
                    },
                },
            }),
            Item::ToolResult { name, output, .. } => messages.push(Message {
                role: "tool".to_owned(),
                content: super::value_text(&output),
                tool_name: Some(name),
                ..Message::default()
            }),
            Item::WebSearch(_) | Item::Opaque(_) => {}
        }
    }
    Ok(messages)
}

fn assistant_message(messages: &mut Vec<Message>) -> &mut Message {
    if messages
        .last()
        .is_none_or(|message| message.role != "assistant")
    {
        messages.push(Message {
            role: "assistant".to_owned(),
            ..Message::default()
        });
    }
    messages.last_mut().expect("assistant message was inserted")
}

#[cfg(test)]
mod tests {
    use std::io;

    use atra_protocol::{
        AssistantMessageEvent, EventSequence, FrozenBoundaryEvent, MessageEvent, OpaqueState,
        ReasoningEvent, ThreadEventData, ToolArtifact, ToolCallEvent, ToolResultEvent,
    };
    use futures_util::{StreamExt, stream};

    use super::*;

    fn event(sequence: i64, data: ThreadEventData) -> Event {
        Event {
            sequence: EventSequence(sequence),
            data,
        }
    }

    fn response(body: impl Into<reqwest::Body>) -> Response {
        http::Response::builder()
            .status(200)
            .body(body.into())
            .unwrap()
            .into()
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
    fn request_history_preserves_an_ollama_assistant_turn() {
        let model = "qwen3:cloud";
        let events = vec![
            event(
                0,
                ThreadEventData::UserMessage(MessageEvent {
                    content: "question".to_owned(),
                }),
            ),
            event(
                1,
                ThreadEventData::Reasoning(ReasoningEvent {
                    summary: "think".to_owned(),
                    opaque: Some(OpaqueState {
                        replay_key: thinking_replay_key(model),
                        payload: json!({"thinking": "think"}),
                    }),
                }),
            ),
            event(
                2,
                ThreadEventData::AssistantMessage(AssistantMessageEvent {
                    content: "checking".to_owned(),
                    phase: AssistantMessagePhase::Commentary,
                    todos: Vec::new(),
                }),
            ),
            event(
                3,
                ThreadEventData::ToolCall(ToolCallEvent::Function {
                    name: "lookup".to_owned(),
                    arguments: json!({"city": "Tokyo"}),
                    call_id: "call-1".to_owned(),
                }),
            ),
            event(
                4,
                ThreadEventData::ToolCall(ToolCallEvent::Function {
                    name: "lookup".to_owned(),
                    arguments: json!({"city": "Osaka"}),
                    call_id: "call-2".to_owned(),
                }),
            ),
            event(
                5,
                ThreadEventData::ToolResult(ToolResultEvent::Function {
                    name: "lookup".to_owned(),
                    call_id: "call-1".to_owned(),
                    result: json!({"temperature": 20}),
                    artifacts: Vec::new(),
                    masked_result: None,
                }),
            ),
            event(
                6,
                ThreadEventData::ToolResult(ToolResultEvent::Function {
                    name: "lookup".to_owned(),
                    call_id: "call-2".to_owned(),
                    result: json!({"temperature": 21}),
                    artifacts: Vec::new(),
                    masked_result: None,
                }),
            ),
        ];

        let messages = event_messages(&events, Some(&thinking_replay_key(model))).unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].thinking, "think");
        assert_eq!(messages[1].content, "checking");
        assert_eq!(messages[1].tool_calls.len(), 2);
        assert_eq!(messages[2].tool_name.as_deref(), Some("lookup"));
        assert_eq!(messages[3].tool_name.as_deref(), Some("lookup"));
        assert!(
            serde_json::to_value(&messages[2])
                .unwrap()
                .get("tool_call_id")
                .is_none()
        );
    }

    #[test]
    fn request_history_does_not_replay_reasoning_for_another_model() {
        let events = vec![event(
            0,
            ThreadEventData::Reasoning(ReasoningEvent {
                summary: "visible summary".to_owned(),
                opaque: Some(OpaqueState {
                    replay_key: thinking_replay_key("other-model"),
                    payload: json!({"thinking": "private thinking"}),
                }),
            }),
        )];

        assert!(
            event_messages(&events, Some(&thinking_replay_key("current-model")))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn request_tool_call_message_serializes_required_empty_content() {
        let messages = event_messages(
            &[event(
                0,
                ThreadEventData::ToolCall(ToolCallEvent::Function {
                    name: "lookup".to_owned(),
                    arguments: json!({}),
                    call_id: "call-1".to_owned(),
                }),
            )],
            None,
        )
        .unwrap();

        let value = serde_json::to_value(messages).unwrap();
        assert_eq!(value[0]["role"], "assistant");
        assert_eq!(value[0]["content"], "");
        assert!(value[0]["tool_calls"].is_array());
    }

    #[test]
    fn context_tokens_use_reasoning_summaries_and_masked_tool_results() {
        let events = vec![
            event(
                0,
                ThreadEventData::Reasoning(ReasoningEvent {
                    summary: "reasoning summary".to_owned(),
                    opaque: None,
                }),
            ),
            event(
                1,
                ThreadEventData::ToolResult(ToolResultEvent::Function {
                    name: "command".to_owned(),
                    call_id: "call".to_owned(),
                    result: Value::String("large output ".repeat(10_000)),
                    artifacts: Vec::<ToolArtifact>::new(),
                    masked_result: Some(Value::String("output masked".to_owned())),
                }),
            ),
            event(
                2,
                ThreadEventData::FrozenBoundary(FrozenBoundaryEvent {
                    through_sequence: EventSequence(1),
                    masked_sequences: vec![EventSequence(1)],
                }),
            ),
        ];

        let full_tokens = context_tokens(&events[..2]).unwrap();
        let masked_tokens = context_tokens(&events).unwrap();

        assert!(masked_tokens < full_tokens);
        assert!(
            event_messages(&events, None).unwrap()[0]
                .thinking
                .contains("reasoning summary")
        );
    }

    #[tokio::test]
    async fn decodes_fragmented_stream_with_thinking_text_and_parallel_tools() {
        let body = br#"{"message":{"role":"assistant","thinking":"th"},"done":false}
{"message":{"role":"assistant","thinking":"ink","content":"checking"},"done":false}
{"message":{"role":"assistant","tool_calls":[{"id":"call-1","function":{"name":"lookup","arguments":{"city":"Tokyo"}}},{"id":"call-2","function":{"name":"lookup","arguments":{"city":"Osaka"}}}]},"done":false}
{"message":{"role":"assistant"},"done":true,"prompt_eval_count":10,"eval_count":4}"#;
        let body = reqwest::Body::wrap_stream(stream::iter(
            body.chunks(17)
                .map(|chunk| Ok::<_, io::Error>(chunk.to_vec())),
        ));

        let events = response_stream(response(body), "session", "qwen3:cloud")
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::Reasoning { summary, .. })
            }) if summary == "think"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, phase })
            }) if content == "checking" && *phase == AssistantMessagePhase::Commentary
        )));
        let calls = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Ok(ModelEvent::OutputItemDone {
                        response: Some(ModelResponse::ToolCall { .. })
                    })
                )
            })
            .count();
        assert_eq!(calls, 2);
        assert!(matches!(
            events.last(),
            Some(Ok(ModelEvent::Completed {
                token_usage: Some(usage),
                ..
            })) if usage["total_tokens"] == 14
        ));
    }

    #[tokio::test]
    async fn rejects_a_stream_without_a_completion_marker() {
        let events = response_stream(
            response(
                "{\"message\":{\"role\":\"assistant\",\"content\":\"partial\"},\"done\":false}\n",
            ),
            "session",
            "model",
        )
        .collect::<Vec<_>>()
        .await;

        assert!(events.last().is_some_and(Result::is_err));
    }

    #[tokio::test]
    async fn rejects_non_object_tool_arguments() {
        let events = response_stream(
            response(
                "{\"message\":{\"role\":\"assistant\",\"tool_calls\":[{\"function\":{\"name\":\"lookup\",\"arguments\":\"bad\"}}]},\"done\":true,\"prompt_eval_count\":1,\"eval_count\":1}\n",
            ),
            "session",
            "model",
        )
        .collect::<Vec<_>>()
        .await;

        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());
    }

    #[tokio::test]
    async fn accepts_an_argumentless_tool_call() {
        let events = response_stream(
            response(
                "{\"message\":{\"role\":\"assistant\",\"tool_calls\":[{\"function\":{\"name\":\"lookup\"}}]},\"done\":true}\n",
            ),
            "session",
            "model",
        )
        .collect::<Vec<_>>()
        .await;

        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::ToolCall { arguments, .. })
            }) if arguments == &json!({})
        )));
        assert!(matches!(
            events.last(),
            Some(Ok(ModelEvent::Completed {
                token_usage: None,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn accepts_completion_without_role_or_token_counts() {
        let events = response_stream(
            response("{\"message\":{\"content\":\"done\"},\"done\":true}\n"),
            "session",
            "model",
        )
        .collect::<Vec<_>>()
        .await;

        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, .. })
            }) if content == "done"
        )));
        assert!(matches!(
            events.last(),
            Some(Ok(ModelEvent::Completed {
                token_usage: None,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn rejects_completion_without_assistant_content_or_tool_calls() {
        let events = response_stream(
            response("{\"message\":{},\"done\":true,\"prompt_eval_count\":1,\"eval_count\":0}\n"),
            "session",
            "model",
        )
        .collect::<Vec<_>>()
        .await;

        assert_eq!(events.len(), 1);
        let Err(error) = &events[0] else {
            panic!("expected empty Ollama completion error");
        };
        assert!(
            error
                .to_string()
                .contains("without assistant content or tool calls")
        );
    }

    #[tokio::test]
    async fn rejects_thinking_only_completion() {
        let events = response_stream(
            response(
                "{\"message\":{\"thinking\":\"unfinished\"},\"done\":true,\"prompt_eval_count\":1,\"eval_count\":1}\n",
            ),
            "session",
            "model",
        )
        .collect::<Vec<_>>()
        .await;

        assert!(events.last().is_some_and(Result::is_err));
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone { .. } | ModelEvent::Completed { .. })
        )));
    }

    #[tokio::test]
    async fn generates_a_call_id_when_ollama_omits_it() {
        let events = response_stream(
            response(
                "{\"message\":{\"role\":\"assistant\",\"tool_calls\":[{\"function\":{\"name\":\"lookup\",\"arguments\":{}}}]},\"done\":true,\"prompt_eval_count\":1,\"eval_count\":1}\n",
            ),
            "session",
            "model",
        )
        .collect::<Vec<_>>()
        .await;

        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::ToolCall { call_id, .. })
            }) if call_id.starts_with("ollama-")
        )));
        assert!(matches!(
            events.last(),
            Some(Ok(ModelEvent::Completed { .. }))
        ));
    }

    #[tokio::test]
    async fn reports_ollama_stream_errors() {
        let events = response_stream(
            response("{\"error\":\"model failed\"}\n"),
            "session",
            "model",
        )
        .collect::<Vec<_>>()
        .await;

        assert_eq!(events.len(), 1);
        let Err(error) = &events[0] else {
            panic!("expected Ollama stream error");
        };
        assert!(error.to_string().contains("model failed"));
    }

    #[test]
    fn rejects_unknown_reasoning_options() {
        assert!(think_value("almost-high").is_err());
    }
}
