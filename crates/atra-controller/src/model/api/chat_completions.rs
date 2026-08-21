use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use futures_util::stream;
use reqwest::Client;
use serde_json::{Value, json};

use super::{chat_messages, function_tools, incomplete_stream, request_stream, required_u64};
use crate::model::{ModelEvent, ModelEventStream, ModelRequest, ModelResponse, ModelStreamEvent};

pub(crate) async fn stream(
    client: &Client,
    url: &str,
    api_key: &str,
    request: &ModelRequest<'_>,
) -> Result<ModelEventStream> {
    let tools = function_tools(request.tools);
    let mut body = json!({
        "model": request.model,
        "messages": chat_messages(request)?,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = Value::String("auto".to_owned());
        body["parallel_tool_calls"] = Value::Bool(true);
    }
    if request.reasoning_effort != "default" {
        body["reasoning_effort"] = Value::String(request.reasoning_effort.to_owned());
    }
    let client = client.clone();
    let url = url.to_owned();
    let api_key = api_key.to_owned();
    let model = request.model.to_owned();
    Ok(request_stream(
        move || {
            let request = client.post(&url).bearer_auth(&api_key).json(&body);
            async move {
                Ok((
                    request
                        .send()
                        .await
                        .context("Chat Completions request failed")?,
                    (),
                ))
            }
        },
        "Chat Completions",
        live,
        move |frames, ()| parse(&frames, &model),
    ))
}

fn live(frame: &Value) -> Result<Vec<ModelStreamEvent>> {
    if frame.get("error").is_some() {
        bail!("Chat Completions error: {}", frame["error"]);
    }
    let Some(delta) = frame.pointer("/choices/0/delta") else {
        return Ok(Vec::new());
    };
    let mut updates = Vec::new();
    if let Some(value) = delta["content"].as_str() {
        updates.push(ModelStreamEvent::AssistantDelta {
            content: value.to_owned(),
            phase: atra_protocol::AssistantMessagePhase::Commentary,
        });
    }
    if let Some(value) = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
    {
        updates.push(ModelStreamEvent::ReasoningSummaryDelta(value.to_owned()));
    }
    Ok(updates)
}

fn parse(frames: &[Value], model: &str) -> Result<ModelEventStream> {
    let mut output = Vec::new();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    let mut usage = None;
    let mut completed = false;
    for frame in frames {
        if frame.get("error").is_some() {
            bail!("Chat Completions error: {}", frame["error"]);
        }
        if let Some(value) = frame.get("usage").filter(|value| !value.is_null()) {
            usage = Some(normalize_usage(value));
        }
        completed |= frame["choices"].as_array().is_some_and(|choices| {
            choices
                .iter()
                .any(|choice| !choice["finish_reason"].is_null())
        });
        let Some(delta) = frame.pointer("/choices/0/delta") else {
            continue;
        };
        if let Some(value) = delta["content"].as_str() {
            text.push_str(value);
            output.push(Ok(ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                content: value.to_owned(),
                phase: atra_protocol::AssistantMessagePhase::Commentary,
            })));
        }
        if let Some(value) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
        {
            reasoning.push_str(value);
            output.push(Ok(ModelEvent::Update(
                ModelStreamEvent::ReasoningSummaryDelta(value.to_owned()),
            )));
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = required_u64(call, "index", "Chat Completions tool call delta")?;
            let entry = calls.entry(index).or_default();
            if let Some(id) = call["id"].as_str() {
                entry.0 = id.to_owned();
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                entry.1 = name.to_owned();
                output.push(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
                    item_id: entry.0.clone(),
                    call_id: Some(entry.0.clone()),
                    name: name.to_owned(),
                })));
            }
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                entry.2.push_str(arguments);
                output.push(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
                    item_id: entry.0.clone(),
                    delta: arguments.to_owned(),
                })));
            }
        }
    }
    if !completed {
        return Err(incomplete_stream("Chat Completions"));
    }
    if !reasoning.is_empty() {
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::Reasoning {
                summary: reasoning,
                opaque: atra_protocol::OpaqueState {
                    replay_key: format!("chat-completions/{}/reasoning-v1", model),
                    payload: Value::Null,
                },
            }),
        }));
    }
    if !text.is_empty() {
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::AssistantMessage {
                content: text,
                phase: if calls.is_empty() {
                    atra_protocol::AssistantMessagePhase::FinalAnswer
                } else {
                    atra_protocol::AssistantMessagePhase::Commentary
                },
            }),
        }));
    }
    for (_, (call_id, name, arguments)) in calls {
        anyhow::ensure!(!call_id.is_empty(), "Chat Completions tool call omitted id");
        anyhow::ensure!(
            !name.is_empty(),
            "Chat Completions tool call omitted function name"
        );
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::ToolCall {
                name,
                arguments: serde_json::from_str(&arguments)
                    .context("Chat Completions returned invalid tool arguments")?,
                call_id,
            }),
        }));
    }
    output.push(Ok(ModelEvent::Completed {
        token_usage: usage,
        rate_limits: Vec::new(),
    }));
    Ok(Box::pin(stream::iter(output)))
}

fn normalize_usage(value: &Value) -> Value {
    json!({
        "input_tokens": value["prompt_tokens"],
        "cached_input_tokens": value.pointer("/prompt_tokens_details/cached_tokens"),
        "output_tokens": value["completion_tokens"],
        "reasoning_output_tokens": value.pointer("/completion_tokens_details/reasoning_tokens"),
        "total_tokens": value["total_tokens"],
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
            json!({"choices":[{"delta":{"reasoning_content":"think"}}]}),
            json!({"choices":[{"delta":{"content":"done"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}}),
        ];
        let events = parse(&frames, "fixture-model")
            .unwrap()
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
            }) if content == "done"
                && *phase == atra_protocol::AssistantMessagePhase::FinalAnswer
        )));
    }

    #[tokio::test]
    async fn parses_scrubbed_live_tool_fixture() {
        let frames = include_str!("fixtures/chat_completions_tool.ndjson")
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<Value>, _>>()
            .unwrap();
        let events = parse(&frames, "deepseek-v4-flash")
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
                && arguments["command"] == "printf 'DEEPSEEK_TOOL'"
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
    fn rejects_invalid_tool_json() {
        let frames = vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"command","arguments":"{"}}]}}]}),
        ];
        assert!(parse(&frames, "fixture-model").is_err());
    }

    #[test]
    fn rejects_tool_calls_without_an_id() {
        let frames = vec![json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"name": "command", "arguments": "{}"}
                    }]
                }
            }]
        })];

        assert!(parse(&frames, "fixture-model").is_err());
    }
}
