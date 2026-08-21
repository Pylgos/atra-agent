use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
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

pub(crate) async fn stream(
    client: &Client,
    url: &str,
    api_key: &str,
    request: &ModelRequest<'_>,
) -> Result<ModelEventStream> {
    let (system, messages) = messages(request)?;
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
            "max_tokens": 16_384,
            "stream": true,
        });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    match request.reasoning_effort {
        "on" => body["thinking"] = json!({"type": "enabled", "budget_tokens": 16_384}),
        "off" => body["thinking"] = json!({"type": "disabled"}),
        "default" => {}
        value => body["reasoning_effort"] = Value::String(value.to_owned()),
    }
    let client = client.clone();
    let url = url.to_owned();
    let api_key = api_key.to_owned();
    let model = request.model.to_owned();
    Ok(request_stream(
        move || {
            let request = client
                .post(&url)
                .bearer_auth(&api_key)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header(
                    "anthropic-beta",
                    "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14",
                )
                .json(&body);
            async move { Ok((request.send().await.context("Messages request failed")?, ())) }
        },
        "Messages",
        live,
        move |frames, ()| parse(&frames, &model),
    ))
}

fn live(frame: &Value) -> Result<Vec<ModelStreamEvent>> {
    match frame["type"].as_str().unwrap_or_default() {
        "error" => bail!("Messages error: {}", frame["error"]),
        "content_block_delta" => match frame.pointer("/delta/type").and_then(Value::as_str) {
            Some("text_delta") => Ok(vec![ModelStreamEvent::AssistantDelta {
                content: required_str(&frame["delta"], "text", "Messages text_delta event")?
                    .to_owned(),
                phase: atra_protocol::AssistantMessagePhase::Commentary,
            }]),
            Some("thinking_delta") => Ok(vec![ModelStreamEvent::ReasoningSummaryDelta(
                required_str(&frame["delta"], "thinking", "Messages thinking_delta event")?
                    .to_owned(),
            )]),
            _ => Ok(Vec::new()),
        },
        _ => Ok(Vec::new()),
    }
}

fn parse(frames: &[Value], model: &str) -> Result<ModelEventStream> {
    let mut output = Vec::new();
    let mut text = String::new();
    let mut thinking = String::new();
    let mut signature = None;
    let mut tools: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    let mut usage = json!({});
    let mut completed = false;
    for frame in frames {
        match frame["type"].as_str().unwrap_or_default() {
            "error" => bail!("Messages error: {}", frame["error"]),
            "message_start" => usage = frame.pointer("/message/usage").cloned().unwrap_or_default(),
            "content_block_start" => {
                let index = required_u64(frame, "index", "Messages content_block_start event")?;
                let block = &frame["content_block"];
                if block["type"] == "tool_use" {
                    let id = required_nonempty_str(block, "id", "Messages tool_use content block")?
                        .to_owned();
                    let name =
                        required_nonempty_str(block, "name", "Messages tool_use content block")?
                            .to_owned();
                    tools.insert(index, (id.clone(), name.clone(), String::new()));
                    output.push(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
                        item_id: id.clone(),
                        call_id: Some(id),
                        name,
                    })));
                }
            }
            "content_block_delta" => {
                let index = required_u64(frame, "index", "Messages content_block_delta event")?;
                let delta = &frame["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let value = required_str(delta, "text", "Messages text_delta event")?;
                        text.push_str(value);
                        output.push(Ok(ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                            content: value.to_owned(),
                            phase: atra_protocol::AssistantMessagePhase::Commentary,
                        })));
                    }
                    "thinking_delta" => {
                        let value =
                            required_str(delta, "thinking", "Messages thinking_delta event")?;
                        thinking.push_str(value);
                        output.push(Ok(ModelEvent::Update(
                            ModelStreamEvent::ReasoningSummaryDelta(value.to_owned()),
                        )));
                    }
                    "signature_delta" => {
                        signature = Some(
                            required_str(delta, "signature", "Messages signature_delta event")?
                                .to_owned(),
                        );
                    }
                    "input_json_delta" => {
                        let value =
                            required_str(delta, "partial_json", "Messages input_json_delta event")?;
                        let call = tools.get_mut(&index).with_context(|| {
                            format!(
                                "Messages input_json_delta referenced unknown content block {index}"
                            )
                        })?;
                        call.2.push_str(value);
                        output.push(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
                            item_id: call.0.clone(),
                            delta: value.to_owned(),
                        })));
                    }
                    other if !other.is_empty() => bail!("unknown Messages delta {other}"),
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(value) = frame.get("usage") {
                    merge_usage(&mut usage, value);
                }
            }
            "message_stop" => completed = true,
            "content_block_stop" | "ping" => {}
            other => bail!("unknown Messages event {other}"),
        }
    }
    if !completed {
        return Err(incomplete_stream("Messages"));
    }
    if !thinking.is_empty() {
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::Reasoning {
                summary: thinking.clone(),
                opaque: atra_protocol::OpaqueState {
                    replay_key: format!("messages/{}/thinking-v1", model),
                    payload: json!({"thinking": thinking, "signature": signature}),
                },
            }),
        }));
    }
    if !text.is_empty() {
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::AssistantMessage {
                content: text,
                phase: if tools.is_empty() {
                    atra_protocol::AssistantMessagePhase::FinalAnswer
                } else {
                    atra_protocol::AssistantMessagePhase::Commentary
                },
            }),
        }));
    }
    for (_, (call_id, name, arguments)) in tools {
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::ToolCall {
                name,
                arguments: serde_json::from_str(&arguments)
                    .context("Messages returned invalid tool arguments")?,
                call_id,
            }),
        }));
    }
    output.push(Ok(ModelEvent::Completed {
        token_usage: Some(normalize_usage(&usage)),
        rate_limits: Vec::new(),
    }));
    Ok(Box::pin(stream::iter(output)))
}

fn messages(request: &ModelRequest<'_>) -> Result<(String, Vec<Value>)> {
    let mut system = request.instructions.to_owned();
    let mut messages = Vec::new();
    for item in super::super::surface::derive(request.events, None)?.items {
        match item {
            Item::Message { role, text, .. } => match role {
                Role::Developer => {
                    system.push_str("\n\n");
                    system.push_str(&text);
                }
                Role::User => {
                    push_content(&mut messages, "user", json!({"type": "text", "text": text}))
                }
                Role::Assistant => push_content(
                    &mut messages,
                    "assistant",
                    json!({"type": "text", "text": text}),
                ),
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
                    && opaque.replay_key == format!("messages/{}/thinking-v1", request.model)
                {
                    push_content(
                        &mut messages,
                        "assistant",
                        json!({
                            "type": "thinking",
                            "thinking": opaque.payload["thinking"],
                            "signature": opaque.payload["signature"],
                        }),
                    );
                }
            }
            Item::WebSearch(_) | Item::Opaque(_) => {}
        }
    }
    Ok((system, messages))
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
    let input = value["input_tokens"].as_i64().unwrap_or(0);
    let output = value["output_tokens"].as_i64().unwrap_or(0);
    json!({
        "input_tokens": input,
        "cached_input_tokens": value["cache_read_input_tokens"],
        "cache_write_input_tokens": value["cache_creation_input_tokens"],
        "output_tokens": output,
        "total_tokens": input.saturating_add(output),
    })
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn parses_reasoning_and_text() {
        let frames = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":8}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"think"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"done"}}),
            json!({"type":"message_delta","usage":{"output_tokens":4}}),
            json!({"type":"message_stop"}),
        ];
        let events = parse(&frames, "fixture-model")
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::Reasoning { summary, opaque })
            }) if summary == "think" && opaque.payload["signature"] == "signed"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, phase })
            }) if content == "done"
                && *phase == atra_protocol::AssistantMessagePhase::FinalAnswer
        )));
    }

    #[tokio::test]
    async fn parses_scrubbed_live_tool_fixture() {
        let frames = include_str!("fixtures/messages_tool.ndjson")
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<Value>, _>>()
            .unwrap();
        let events = parse(&frames, "minimax-m3")
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::ToolCall {
                    call_id,
                    arguments,
                    ..
                })
            }) if call_id == "call_fixture"
                && arguments["command"] == "printf 'MESSAGES_TOOL'"
        )));
        assert!(matches!(
            events.last(),
            Some(Ok(ModelEvent::Completed {
                token_usage: Some(_),
                ..
            }))
        ));
    }

    #[test]
    fn rejects_tool_blocks_without_an_id() {
        assert!(
            parse(
                &[json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "tool_use", "name": "command"}
                })],
                "fixture-model",
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_stream_event() {
        assert!(parse(&[json!({"type":"new_event"})], "fixture-model").is_err());
    }
}
