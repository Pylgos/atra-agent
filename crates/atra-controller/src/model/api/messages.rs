use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use futures_util::stream;
use reqwest::Client;
use serde_json::{Value, json};

use super::{
    function_tools, incomplete_stream, request_stream, required_nonempty_str, required_str,
    required_u64, value_text,
};
use crate::model::{
    ModelEvent, ModelEventStream, ModelRequest, ModelResponse, ModelStreamEvent,
    surface::{Item, Role, ToolInput},
};

const MAX_OUTPUT_TOKENS: u64 = 65_536;

pub(crate) async fn stream(
    client: &Client,
    url: &str,
    api_key: &str,
    request: &ModelRequest<'_>,
) -> Result<ModelEventStream> {
    let body = request_body(request)?;
    let client = client.clone();
    let url = url.to_owned();
    let api_key = api_key.to_owned();
    let model = request.model.to_owned();
    let mut live = Live::default();
    Ok(request_stream(
        move || {
            let request = client
                .post(&url)
                .bearer_auth(&api_key)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", "fine-grained-tool-streaming-2025-05-14")
                .json(&body);
            async move { Ok((request.send().await.context("Messages request failed")?, ())) }
        },
        "Messages",
        move |frame| live.decode(frame),
        move |frames, ()| Message::decode(&frames, &model)?.into_stream(),
    ))
}

fn request_body(request: &ModelRequest<'_>) -> Result<Value> {
    let (system, messages) = request_messages(request)?;
    let tools = function_tools(request.tools)
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.pointer("/function/name"),
                "description": tool.pointer("/function/description"),
                "input_schema": tool.pointer("/function/parameters"),
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": request.model,
        "system": system,
        "messages": messages,
        "max_tokens": MAX_OUTPUT_TOKENS,
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    match request.reasoning_effort {
        "default" => {}
        effort @ ("low" | "medium" | "high" | "xhigh" | "max") => {
            body["thinking"] = json!({"type": "adaptive"});
            body["output_config"] = json!({"effort": effort});
        }
        effort => bail!("unsupported Messages reasoning effort {effort}"),
    }
    Ok(body)
}

#[derive(Default)]
struct Live {
    blocks: BTreeMap<u64, LiveBlock>,
}

enum LiveBlock {
    Text,
    Thinking,
    RedactedThinking,
    ToolUse { call_id: String },
}

impl Live {
    fn decode(&mut self, frame: &Value) -> Result<Vec<ModelEvent>> {
        match event_type(frame)? {
            "error" => bail!("Messages error: {}", frame["error"]),
            "message_start" => {
                self.blocks.clear();
                Ok(Vec::new())
            }
            "content_block_start" => self.start_block(frame),
            "content_block_delta" => self.delta(frame),
            "content_block_stop" => {
                let index = required_u64(frame, "index", "Messages content_block_stop event")?;
                ensure!(
                    self.blocks.remove(&index).is_some(),
                    "Messages stopped unknown content block {index}"
                );
                Ok(Vec::new())
            }
            "message_delta" | "message_stop" | "ping" => Ok(Vec::new()),
            other => bail!("unknown Messages event {other}"),
        }
    }

    fn start_block(&mut self, frame: &Value) -> Result<Vec<ModelEvent>> {
        let index = required_u64(frame, "index", "Messages content_block_start event")?;
        ensure!(
            !self.blocks.contains_key(&index),
            "Messages started duplicate content block {index}"
        );
        let block = &frame["content_block"];
        let (block, update) =
            match required_str(block, "type", "Messages content_block_start event")? {
                "text" => (LiveBlock::Text, None),
                "thinking" => (LiveBlock::Thinking, None),
                "redacted_thinking" => (LiveBlock::RedactedThinking, None),
                "tool_use" => {
                    let call_id =
                        required_nonempty_str(block, "id", "Messages tool_use content block")?
                            .to_owned();
                    let name =
                        required_nonempty_str(block, "name", "Messages tool_use content block")?
                            .to_owned();
                    (
                        LiveBlock::ToolUse {
                            call_id: call_id.clone(),
                        },
                        Some(ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
                            item_id: call_id.clone(),
                            call_id: Some(call_id),
                            name,
                        })),
                    )
                }
                other => bail!("unknown Messages content block {other}"),
            };
        self.blocks.insert(index, block);
        Ok(update.into_iter().collect())
    }

    fn delta(&self, frame: &Value) -> Result<Vec<ModelEvent>> {
        let index = required_u64(frame, "index", "Messages content_block_delta event")?;
        let block = self
            .blocks
            .get(&index)
            .with_context(|| format!("Messages delta referenced unknown content block {index}"))?;
        let delta = &frame["delta"];
        Ok(
            match required_str(delta, "type", "Messages content_block_delta event")? {
                "text_delta" => {
                    ensure!(
                        matches!(block, LiveBlock::Text),
                        "Messages text_delta referenced non-text content block {index}"
                    );
                    vec![ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                        content: required_str(delta, "text", "Messages text_delta event")?
                            .to_owned(),
                        phase: atra_protocol::AssistantMessagePhase::Commentary,
                    })]
                }
                "thinking_delta" => {
                    ensure!(
                        matches!(block, LiveBlock::Thinking),
                        "Messages thinking_delta referenced non-thinking content block {index}"
                    );
                    vec![ModelEvent::Update(ModelStreamEvent::ReasoningSummaryDelta(
                        required_str(delta, "thinking", "Messages thinking_delta event")?
                            .to_owned(),
                    ))]
                }
                "signature_delta" => {
                    ensure!(
                        matches!(block, LiveBlock::Thinking),
                        "Messages signature_delta referenced non-thinking content block {index}"
                    );
                    required_str(delta, "signature", "Messages signature_delta event")?;
                    Vec::new()
                }
                "input_json_delta" => {
                    let LiveBlock::ToolUse { call_id } = block else {
                        bail!(
                            "Messages input_json_delta referenced non-tool content block {index}"
                        );
                    };
                    vec![ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
                        item_id: call_id.clone(),
                        delta: required_str(
                            delta,
                            "partial_json",
                            "Messages input_json_delta event",
                        )?
                        .to_owned(),
                    })]
                }
                "citations_delta" => {
                    ensure!(
                        matches!(block, LiveBlock::Text),
                        "Messages citations_delta referenced non-text content block {index}"
                    );
                    ensure!(
                        delta["citation"].is_object(),
                        "Messages citations_delta event omitted object field citation"
                    );
                    Vec::new()
                }
                other => bail!("unknown Messages delta {other}"),
            },
        )
    }
}

struct Message {
    blocks: BTreeMap<u64, Block>,
    usage: Value,
    stop_reason: StopReason,
    model: String,
}

struct OpenBlock {
    content: Block,
    stopped: bool,
}

enum Block {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        call_id: String,
        name: String,
        initial_arguments: Value,
        argument_delta: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StopReason {
    EndTurn,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    MaxTokens,
    ContextWindow,
}

impl Message {
    fn decode(frames: &[Value], model: &str) -> Result<Self> {
        let mut blocks: BTreeMap<u64, OpenBlock> = BTreeMap::new();
        let mut usage = json!({});
        let mut stop_reason = None;
        let mut started = false;
        let mut stopped = false;
        for frame in frames {
            match event_type(frame)? {
                "error" => bail!("Messages error: {}", frame["error"]),
                "message_start" => {
                    ensure!(!started, "Messages returned multiple message_start events");
                    ensure!(!stopped, "Messages started after message_stop");
                    started = true;
                    usage = frame.pointer("/message/usage").cloned().unwrap_or_default();
                }
                "content_block_start" => {
                    ensure!(started, "Messages content block preceded message_start");
                    ensure!(!stopped, "Messages content block followed message_stop");
                    start_block(&mut blocks, frame)?;
                }
                "content_block_delta" => {
                    ensure!(!stopped, "Messages delta followed message_stop");
                    apply_delta(&mut blocks, frame)?;
                }
                "content_block_stop" => {
                    ensure!(!stopped, "Messages content block followed message_stop");
                    stop_block(&mut blocks, frame)?;
                }
                "message_delta" => {
                    ensure!(started, "Messages message_delta preceded message_start");
                    ensure!(!stopped, "Messages message_delta followed message_stop");
                    if let Some(value) = frame.get("usage") {
                        merge_usage(&mut usage, value);
                    }
                    let value = frame["delta"]
                        .get("stop_reason")
                        .context("Messages message_delta event omitted field stop_reason")?;
                    if !value.is_null() {
                        let value = value.as_str().context(
                            "Messages message_delta event field stop_reason was not a string or null",
                        )?;
                        ensure!(
                            stop_reason.is_none(),
                            "Messages returned multiple stop reasons"
                        );
                        stop_reason = Some(StopReason::parse(value)?);
                    }
                }
                "message_stop" => {
                    ensure!(started, "Messages message_stop preceded message_start");
                    ensure!(!stopped, "Messages returned multiple message_stop events");
                    stopped = true;
                }
                "ping" => {}
                other => bail!("unknown Messages event {other}"),
            }
        }
        if !stopped {
            return Err(incomplete_stream("Messages"));
        }
        ensure!(
            blocks.values().all(|block| block.stopped),
            "Messages stopped with an open content block"
        );
        let stop_reason = stop_reason.context("Messages response omitted stop_reason")?;
        let blocks = blocks
            .into_iter()
            .map(|(index, block)| (index, block.content))
            .collect();
        Ok(Self {
            blocks,
            usage,
            stop_reason,
            model: model.to_owned(),
        })
    }

    fn into_stream(self) -> Result<ModelEventStream> {
        match self.stop_reason {
            StopReason::MaxTokens => bail!("Messages response reached max_tokens"),
            StopReason::ContextWindow => {
                bail!("Messages response exceeded the model context window")
            }
            _ => {}
        }
        let has_tool = self
            .blocks
            .values()
            .any(|block| matches!(block, Block::ToolUse { .. }));
        ensure!(
            self.stop_reason != StopReason::ToolUse || has_tool,
            "Messages stopped for tool_use without a tool block"
        );
        ensure!(
            self.stop_reason == StopReason::ToolUse || !has_tool,
            "Messages returned a tool block with stop reason other than tool_use"
        );
        let phase = if matches!(
            self.stop_reason,
            StopReason::EndTurn | StopReason::StopSequence | StopReason::Refusal
        ) {
            atra_protocol::AssistantMessagePhase::FinalAnswer
        } else {
            atra_protocol::AssistantMessagePhase::Commentary
        };
        let replay_key = format!("messages/{}/thinking-v2", self.model);
        let mut output = Vec::new();
        let mut assistant_message = false;
        for (_, block) in self.blocks {
            match block {
                Block::Text(content) => {
                    assistant_message = true;
                    output.push(Ok(ModelEvent::OutputItemDone {
                        response: Some(ModelResponse::AssistantMessage { content, phase }),
                    }));
                }
                Block::Thinking {
                    thinking,
                    signature,
                } => output.push(Ok(reasoning_event(
                    thinking.clone(),
                    replay_key.clone(),
                    json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }),
                ))),
                Block::RedactedThinking { data } => output.push(Ok(reasoning_event(
                    String::new(),
                    replay_key.clone(),
                    json!({"type": "redacted_thinking", "data": data}),
                ))),
                Block::ToolUse {
                    call_id,
                    name,
                    initial_arguments,
                    argument_delta,
                } => output.push(Ok(ModelEvent::OutputItemDone {
                    response: Some(ModelResponse::ToolCall {
                        name,
                        arguments: tool_arguments(&initial_arguments, &argument_delta)?,
                        call_id,
                    }),
                })),
            }
        }
        if !assistant_message
            && matches!(
                self.stop_reason,
                StopReason::EndTurn | StopReason::StopSequence | StopReason::Refusal
            )
        {
            output.push(Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage {
                    content: String::new(),
                    phase,
                }),
            }));
        }
        output.push(Ok(ModelEvent::Completed {
            token_usage: Some(normalize_usage(&self.usage)),
            rate_limits: Vec::new(),
        }));
        Ok(Box::pin(stream::iter(output)))
    }
}

impl StopReason {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "end_turn" => Self::EndTurn,
            "stop_sequence" => Self::StopSequence,
            "tool_use" => Self::ToolUse,
            "pause_turn" => Self::PauseTurn,
            "refusal" => Self::Refusal,
            "max_tokens" => Self::MaxTokens,
            "model_context_window_exceeded" => Self::ContextWindow,
            other => bail!("unknown Messages stop reason {other}"),
        })
    }
}

fn start_block(blocks: &mut BTreeMap<u64, OpenBlock>, frame: &Value) -> Result<()> {
    let index = required_u64(frame, "index", "Messages content_block_start event")?;
    ensure!(
        !blocks.contains_key(&index),
        "Messages started duplicate content block {index}"
    );
    let value = &frame["content_block"];
    let content = match required_str(value, "type", "Messages content_block_start event")? {
        "text" => {
            Block::Text(required_str(value, "text", "Messages text content block")?.to_owned())
        }
        "thinking" => Block::Thinking {
            thinking: required_str(value, "thinking", "Messages thinking content block")?
                .to_owned(),
            signature: required_str(value, "signature", "Messages thinking content block")?
                .to_owned(),
        },
        "redacted_thinking" => Block::RedactedThinking {
            data: required_nonempty_str(value, "data", "Messages redacted_thinking content block")?
                .to_owned(),
        },
        "tool_use" => {
            ensure!(
                value["input"].is_object(),
                "Messages tool_use content block omitted object field input"
            );
            Block::ToolUse {
                call_id: required_nonempty_str(value, "id", "Messages tool_use content block")?
                    .to_owned(),
                name: required_nonempty_str(value, "name", "Messages tool_use content block")?
                    .to_owned(),
                initial_arguments: value["input"].clone(),
                argument_delta: String::new(),
            }
        }
        other => bail!("unknown Messages content block {other}"),
    };
    blocks.insert(
        index,
        OpenBlock {
            content,
            stopped: false,
        },
    );
    Ok(())
}

fn apply_delta(blocks: &mut BTreeMap<u64, OpenBlock>, frame: &Value) -> Result<()> {
    let index = required_u64(frame, "index", "Messages content_block_delta event")?;
    let block = blocks
        .get_mut(&index)
        .with_context(|| format!("Messages delta referenced unknown content block {index}"))?;
    ensure!(
        !block.stopped,
        "Messages delta followed content_block_stop for block {index}"
    );
    let delta = &frame["delta"];
    match required_str(delta, "type", "Messages content_block_delta event")? {
        "text_delta" => {
            let Block::Text(text) = &mut block.content else {
                bail!("Messages text_delta referenced non-text content block {index}");
            };
            text.push_str(required_str(delta, "text", "Messages text_delta event")?);
        }
        "thinking_delta" => {
            let Block::Thinking { thinking, .. } = &mut block.content else {
                bail!("Messages thinking_delta referenced non-thinking content block {index}");
            };
            thinking.push_str(required_str(
                delta,
                "thinking",
                "Messages thinking_delta event",
            )?);
        }
        "signature_delta" => {
            let Block::Thinking { signature, .. } = &mut block.content else {
                bail!("Messages signature_delta referenced non-thinking content block {index}");
            };
            signature.push_str(required_str(
                delta,
                "signature",
                "Messages signature_delta event",
            )?);
        }
        "input_json_delta" => {
            let Block::ToolUse { argument_delta, .. } = &mut block.content else {
                bail!("Messages input_json_delta referenced non-tool content block {index}");
            };
            argument_delta.push_str(required_str(
                delta,
                "partial_json",
                "Messages input_json_delta event",
            )?);
        }
        "citations_delta" => {
            ensure!(
                matches!(&block.content, Block::Text(_)),
                "Messages citations_delta referenced non-text content block {index}"
            );
            ensure!(
                delta["citation"].is_object(),
                "Messages citations_delta event omitted object field citation"
            );
        }
        other => bail!("unknown Messages delta {other}"),
    }
    Ok(())
}

fn stop_block(blocks: &mut BTreeMap<u64, OpenBlock>, frame: &Value) -> Result<()> {
    let index = required_u64(frame, "index", "Messages content_block_stop event")?;
    let block = blocks
        .get_mut(&index)
        .with_context(|| format!("Messages stopped unknown content block {index}"))?;
    ensure!(
        !block.stopped,
        "Messages stopped content block {index} more than once"
    );
    if let Block::ToolUse {
        initial_arguments,
        argument_delta,
        ..
    } = &block.content
    {
        tool_arguments(initial_arguments, argument_delta)?;
    }
    if let Block::Thinking { signature, .. } = &block.content {
        ensure!(
            !signature.is_empty(),
            "Messages thinking content block omitted signature"
        );
    }
    block.stopped = true;
    Ok(())
}

fn tool_arguments(initial: &Value, delta: &str) -> Result<Value> {
    let arguments = if delta.is_empty() {
        initial.clone()
    } else {
        serde_json::from_str(delta).context("Messages returned invalid tool arguments")?
    };
    ensure!(
        arguments.is_object(),
        "Messages returned non-object tool arguments"
    );
    Ok(arguments)
}

fn reasoning_event(summary: String, replay_key: String, payload: Value) -> ModelEvent {
    ModelEvent::OutputItemDone {
        response: Some(ModelResponse::Reasoning {
            summary,
            opaque: atra_protocol::OpaqueState {
                replay_key,
                payload,
            },
        }),
    }
}

fn event_type(frame: &Value) -> Result<&str> {
    required_str(frame, "type", "Messages stream event")
}

fn request_messages(request: &ModelRequest<'_>) -> Result<(String, Vec<Value>)> {
    let mut system = request.instructions.to_owned();
    let mut messages = Vec::new();
    let replay_key = format!("messages/{}/thinking-v2", request.model);
    for item in super::super::surface::derive(request.events, None)?.items {
        match item {
            Item::Message { role, text, .. } => match role {
                Role::Developer => {
                    system.push_str("\n\n");
                    system.push_str(&text);
                }
                Role::User if !text.is_empty() => {
                    push_content(&mut messages, "user", json!({"type": "text", "text": text}))
                }
                Role::Assistant if !text.is_empty() => push_content(
                    &mut messages,
                    "assistant",
                    json!({"type": "text", "text": text}),
                ),
                Role::User | Role::Assistant => {}
            },
            Item::ToolCall {
                call_id,
                name,
                input,
                ..
            } => push_content(
                &mut messages,
                "assistant",
                json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": match input {
                        ToolInput::Json(value) => value,
                        ToolInput::Text(value) => json!({"input": value}),
                    }
                }),
            ),
            Item::ToolResult {
                call_id, output, ..
            } => push_content(
                &mut messages,
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": value_text(&output),
                }),
            ),
            Item::Reasoning { opaque, .. } => {
                if let Some(opaque) = opaque
                    && opaque.replay_key == replay_key
                {
                    validate_replay_block(&opaque.payload)?;
                    push_content(&mut messages, "assistant", opaque.payload);
                }
            }
            Item::WebSearch(_) | Item::Opaque(_) => {}
        }
    }
    Ok((system, messages))
}

fn validate_replay_block(block: &Value) -> Result<()> {
    match required_str(block, "type", "Messages opaque thinking block")? {
        "thinking" => {
            required_str(block, "thinking", "Messages opaque thinking block")?;
            required_nonempty_str(block, "signature", "Messages opaque thinking block")?;
        }
        "redacted_thinking" => {
            required_nonempty_str(block, "data", "Messages opaque redacted_thinking block")?;
        }
        other => bail!("unknown Messages opaque thinking block {other}"),
    }
    Ok(())
}

fn push_content(messages: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = messages.last_mut()
        && last["role"] == role
    {
        last["content"]
            .as_array_mut()
            .expect("Messages content is always an array")
            .push(block);
        return;
    }
    messages.push(json!({"role": role, "content": [block]}));
}

fn merge_usage(target: &mut Value, update: &Value) {
    if let (Some(target), Some(update)) = (target.as_object_mut(), update.as_object()) {
        target.extend(update.clone());
    }
}

fn normalize_usage(value: &Value) -> Value {
    let input = usage_value(value, "input_tokens");
    let cached = usage_value(value, "cache_read_input_tokens");
    let cache_write = usage_value(value, "cache_creation_input_tokens");
    let output = usage_value(value, "output_tokens");
    json!({
        "input_tokens": input,
        "cached_input_tokens": cached,
        "cache_write_input_tokens": cache_write,
        "output_tokens": output,
        "total_tokens": input
            .saturating_add(cached)
            .saturating_add(cache_write)
            .saturating_add(output),
    })
}

fn usage_value(value: &Value, field: &str) -> i64 {
    value[field].as_i64().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use serde_json::json;

    use super::*;

    fn block_start(index: u64, block: Value) -> Value {
        json!({"type": "content_block_start", "index": index, "content_block": block})
    }

    fn block_delta(index: u64, delta: Value) -> Value {
        json!({"type": "content_block_delta", "index": index, "delta": delta})
    }

    fn block_stop(index: u64) -> Value {
        json!({"type": "content_block_stop", "index": index})
    }

    fn complete(mut frames: Vec<Value>, stop_reason: &str) -> Vec<Value> {
        frames.insert(
            0,
            json!({"type":"message_start","message":{"usage":{"input_tokens":8}}}),
        );
        frames.push(json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason},
            "usage": {"output_tokens": 4}
        }));
        frames.push(json!({"type": "message_stop"}));
        frames
    }

    async fn events(frames: Vec<Value>) -> Result<Vec<ModelEvent>> {
        Ok(Message::decode(&frames, "fixture-model")?
            .into_stream()?
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?)
    }

    #[test]
    fn sends_effort_with_adaptive_thinking() {
        let request = ModelRequest {
            model: "fixture-model",
            reasoning_effort: "xhigh",
            instructions: "instructions",
            tools: &[],
            events: &[],
            prompt_cache_key: "cache",
        };

        let body = request_body(&request).unwrap();

        assert_eq!(body["max_tokens"], MAX_OUTPUT_TOKENS);
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(body["output_config"], json!({"effort": "xhigh"}));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn rejects_unknown_effort() {
        let request = ModelRequest {
            model: "fixture-model",
            reasoning_effort: "on",
            instructions: "instructions",
            tools: &[],
            events: &[],
            prompt_cache_key: "cache",
        };

        assert!(request_body(&request).is_err());
    }

    #[test]
    fn live_state_resets_when_a_retried_response_starts() {
        let mut live = Live::default();
        let start = block_start(0, json!({"type":"text","text":""}));

        live.decode(&json!({"type":"message_start"})).unwrap();
        live.decode(&start).unwrap();
        live.decode(&json!({"type":"message_start"})).unwrap();

        live.decode(&start).unwrap();
    }

    #[test]
    fn replays_matching_thinking_blocks_without_changing_them() {
        use atra_protocol::{
            AssistantMessageEvent, AssistantMessagePhase, EventSequence, OpaqueState,
            ReasoningEvent, ThreadEventData,
        };

        let thinking = json!({"type":"thinking","thinking":"first","signature":"signature"});
        let redacted = json!({"type":"redacted_thinking","data":"encrypted"});
        let replay_key = "messages/fixture-model/thinking-v2";
        let events = vec![
            crate::storage::Event {
                sequence: EventSequence(0),
                data: ThreadEventData::Reasoning(ReasoningEvent {
                    summary: "first".to_owned(),
                    opaque: Some(OpaqueState {
                        replay_key: replay_key.to_owned(),
                        payload: thinking.clone(),
                    }),
                }),
            },
            crate::storage::Event {
                sequence: EventSequence(1),
                data: ThreadEventData::AssistantMessage(AssistantMessageEvent {
                    content: "commentary".to_owned(),
                    phase: AssistantMessagePhase::Commentary,
                    todos: Vec::new(),
                }),
            },
            crate::storage::Event {
                sequence: EventSequence(2),
                data: ThreadEventData::Reasoning(ReasoningEvent {
                    summary: String::new(),
                    opaque: Some(OpaqueState {
                        replay_key: replay_key.to_owned(),
                        payload: redacted.clone(),
                    }),
                }),
            },
        ];
        let request = ModelRequest {
            model: "fixture-model",
            reasoning_effort: "high",
            instructions: "instructions",
            tools: &[],
            events: &events,
            prompt_cache_key: "cache",
        };

        let (_, messages) = request_messages(&request).unwrap();

        assert_eq!(
            messages,
            vec![json!({
                "role": "assistant",
                "content": [
                    thinking,
                    {"type":"text","text":"commentary"},
                    redacted,
                ]
            })]
        );
    }

    #[test]
    fn omits_empty_text_blocks_from_requests() {
        use atra_protocol::{
            AssistantMessageEvent, AssistantMessagePhase, EventSequence, MessageEvent,
            ThreadEventData,
        };

        let events = vec![
            crate::storage::Event {
                sequence: EventSequence(0),
                data: ThreadEventData::AssistantMessage(AssistantMessageEvent {
                    content: String::new(),
                    phase: AssistantMessagePhase::FinalAnswer,
                    todos: Vec::new(),
                }),
            },
            crate::storage::Event {
                sequence: EventSequence(1),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: String::new(),
                }),
            },
            crate::storage::Event {
                sequence: EventSequence(2),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "next".to_owned(),
                }),
            },
        ];
        let request = ModelRequest {
            model: "fixture-model",
            reasoning_effort: "default",
            instructions: "instructions",
            tools: &[],
            events: &events,
            prompt_cache_key: "cache",
        };

        let (_, messages) = request_messages(&request).unwrap();

        assert_eq!(
            messages,
            vec![json!({
                "role": "user",
                "content": [{"type":"text","text":"next"}],
            })]
        );
    }

    #[tokio::test]
    async fn parses_reasoning_and_text() {
        let frames = complete(
            vec![
                block_start(0, json!({"type":"thinking","thinking":"","signature":""})),
                block_delta(0, json!({"type":"thinking_delta","thinking":"think"})),
                block_delta(0, json!({"type":"signature_delta","signature":"signed"})),
                block_stop(0),
                block_start(1, json!({"type":"text","text":""})),
                block_delta(1, json!({"type":"text_delta","text":"done"})),
                block_stop(1),
            ],
            "end_turn",
        );
        let events = events(frames).await.unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::Reasoning { summary, opaque })
            } if summary == "think"
                && opaque.payload == json!({
                    "type": "thinking",
                    "thinking": "think",
                    "signature": "signed"
                })
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, phase })
            } if content == "done"
                && *phase == atra_protocol::AssistantMessagePhase::FinalAnswer
        )));
    }

    #[tokio::test]
    async fn preserves_each_thinking_block_and_redacted_thinking() {
        let frames = complete(
            vec![
                block_start(
                    0,
                    json!({"type":"thinking","thinking":"first","signature":"sig-1"}),
                ),
                block_stop(0),
                block_start(1, json!({"type":"redacted_thinking","data":"encrypted"})),
                block_stop(1),
                block_start(
                    2,
                    json!({"type":"thinking","thinking":"","signature":"sig-2"}),
                ),
                block_stop(2),
                block_start(3, json!({"type":"text","text":"done"})),
                block_stop(3),
            ],
            "end_turn",
        );
        let events = events(frames).await.unwrap();
        let payloads = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::OutputItemDone {
                    response: Some(ModelResponse::Reasoning { opaque, .. }),
                } => Some(opaque.payload.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            payloads,
            vec![
                json!({"type":"thinking","thinking":"first","signature":"sig-1"}),
                json!({"type":"redacted_thinking","data":"encrypted"}),
                json!({"type":"thinking","thinking":"","signature":"sig-2"}),
            ]
        );
        for payload in payloads {
            validate_replay_block(&payload).unwrap();
        }
    }

    #[test]
    fn rejects_truncated_responses_instead_of_finalizing_text() {
        let frames = complete(
            vec![
                block_start(0, json!({"type":"text","text":"partial"})),
                block_stop(0),
            ],
            "max_tokens",
        );

        assert!(
            Message::decode(&frames, "fixture-model")
                .unwrap()
                .into_stream()
                .is_err()
        );
    }

    #[tokio::test]
    async fn pause_turn_keeps_text_as_commentary() {
        let frames = complete(
            vec![
                block_start(0, json!({"type":"text","text":"continuing"})),
                block_stop(0),
            ],
            "pause_turn",
        );
        let events = events(frames).await.unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { phase, .. })
            } if *phase == atra_protocol::AssistantMessagePhase::Commentary
        )));
    }

    #[tokio::test]
    async fn parses_scrubbed_live_tool_fixture() {
        let frames = include_str!("fixtures/messages_tool.ndjson")
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<Value>, _>>()
            .unwrap();
        let events = events(frames).await.unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::ToolCall {
                    call_id,
                    arguments,
                    ..
                })
            } if call_id == "call_fixture"
                && arguments["command"] == "printf 'MESSAGES_TOOL'"
        )));
        assert!(matches!(
            events.last(),
            Some(ModelEvent::Completed {
                token_usage: Some(_),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn uses_tool_input_from_block_start_when_there_are_no_deltas() {
        for arguments in [json!({}), json!({"query":"Tokyo"})] {
            let frames = complete(
                vec![
                    block_start(
                        0,
                        json!({
                            "type":"tool_use",
                            "id":"call-1",
                            "name":"lookup",
                            "input":arguments,
                        }),
                    ),
                    block_stop(0),
                ],
                "tool_use",
            );

            let output = events(frames).await.unwrap();

            assert!(output.iter().any(|event| matches!(
                event,
                ModelEvent::OutputItemDone {
                    response: Some(ModelResponse::ToolCall {
                        arguments: actual,
                        ..
                    })
                } if actual == &arguments
            )));
        }
    }

    #[test]
    fn rejects_non_object_tool_arguments() {
        let frames = complete(
            vec![
                block_start(
                    0,
                    json!({
                        "type":"tool_use",
                        "id":"call-1",
                        "name":"lookup",
                        "input":{},
                    }),
                ),
                block_delta(0, json!({"type":"input_json_delta","partial_json":"[]"})),
                block_stop(0),
            ],
            "tool_use",
        );

        assert!(Message::decode(&frames, "fixture-model").is_err());
    }

    #[tokio::test]
    async fn accepts_citation_deltas_without_exposing_them() {
        let frames = complete(
            vec![
                block_start(0, json!({"type":"text","text":""})),
                block_delta(
                    0,
                    json!({
                        "type":"citations_delta",
                        "citation":{"type":"char_location"}
                    }),
                ),
                block_delta(0, json!({"type":"text_delta","text":"cited"})),
                block_stop(0),
            ],
            "end_turn",
        );
        let mut live = Live::default();
        for frame in &frames {
            live.decode(frame).unwrap();
        }

        let events = events(frames).await.unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, .. })
            } if content == "cited"
        )));
    }

    #[tokio::test]
    async fn accepts_null_stop_reason_before_the_final_message_delta() {
        let mut frames = complete(
            vec![
                block_start(0, json!({"type":"text","text":"done"})),
                block_stop(0),
            ],
            "end_turn",
        );
        frames.insert(
            frames.len() - 2,
            json!({
                "type":"message_delta",
                "delta":{
                    "container":null,
                    "stop_details":null,
                    "stop_reason":null,
                    "stop_sequence":null
                },
                "usage":{"output_tokens":2}
            }),
        );

        let events = events(frames).await.unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, .. })
            } if content == "done"
        )));
    }

    #[test]
    fn includes_cached_tokens_in_total_usage() {
        assert_eq!(
            normalize_usage(&json!({
                "input_tokens": 10,
                "cache_read_input_tokens": 20,
                "cache_creation_input_tokens": 30,
                "output_tokens": 4,
            })),
            json!({
                "input_tokens": 10,
                "cached_input_tokens": 20,
                "cache_write_input_tokens": 30,
                "output_tokens": 4,
                "total_tokens": 64,
            })
        );
    }

    #[test]
    fn rejects_mismatched_delta_and_block_types() {
        let frames = complete(
            vec![
                block_start(0, json!({"type":"text","text":""})),
                block_delta(0, json!({"type":"input_json_delta","partial_json":"{}"})),
                block_stop(0),
            ],
            "end_turn",
        );

        assert!(Message::decode(&frames, "fixture-model").is_err());
    }

    #[test]
    fn rejects_unknown_stream_event() {
        assert!(Message::decode(&[json!({"type":"new_event"})], "fixture-model").is_err());
    }
}
