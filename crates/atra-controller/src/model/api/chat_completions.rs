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
    let mut live = Live::default();
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
        move |frame| live.decode(frame),
        move |frames, ()| parse(&frames, &model),
    ))
}

#[derive(Default)]
struct Live {
    response_id: Option<String>,
    calls: BTreeMap<u64, ToolCall>,
}

impl Live {
    fn decode(&mut self, frame: &Value) -> Result<Vec<ModelEvent>> {
        check_error(frame)?;
        if let Some(response_id) = frame["id"].as_str() {
            if self
                .response_id
                .as_deref()
                .is_some_and(|id| id != response_id)
            {
                self.calls.clear();
            }
            self.response_id = Some(response_id.to_owned());
        }
        let Some(delta) = frame.pointer("/choices/0/delta") else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        if let Some(value) = delta["content"].as_str().filter(|value| !value.is_empty()) {
            events.push(ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                content: value.to_owned(),
                phase: atra_protocol::AssistantMessagePhase::Commentary,
            }));
        }
        if let Some(value) = delta["refusal"].as_str().filter(|value| !value.is_empty()) {
            events.push(ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                content: value.to_owned(),
                phase: atra_protocol::AssistantMessagePhase::Commentary,
            }));
        }
        if let Some(value) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
            .filter(|value| !value.is_empty())
        {
            events.push(ModelEvent::Update(ModelStreamEvent::ReasoningSummaryDelta(
                value.to_owned(),
            )));
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            events.extend(apply_tool_delta(&mut self.calls, call, true)?);
        }
        Ok(events)
    }
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
    streamed_arguments: usize,
    started: bool,
}

fn apply_tool_delta(
    calls: &mut BTreeMap<u64, ToolCall>,
    delta: &Value,
    streaming: bool,
) -> Result<Vec<ModelEvent>> {
    let context = "Chat Completions tool call delta";
    let index = required_u64(delta, "index", context)?;
    let call = calls.entry(index).or_default();
    if let Some(id) = optional_str(delta, "/id", context)? {
        anyhow::ensure!(!id.is_empty(), "{context} returned an empty id");
        anyhow::ensure!(
            call.id.is_empty() || call.id == id,
            "{context} changed id for index {index}"
        );
        call.id = id.to_owned();
    }
    if let Some(name) = optional_str(delta, "/function/name", context)? {
        anyhow::ensure!(
            !name.is_empty(),
            "{context} returned an empty function name"
        );
        anyhow::ensure!(
            call.name.is_empty() || call.name == name,
            "{context} changed function name for index {index}"
        );
        call.name = name.to_owned();
    }
    let mut events = Vec::new();
    if !call.started && !call.id.is_empty() && !call.name.is_empty() {
        call.started = true;
        events.push(ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
            item_id: call.id.clone(),
            call_id: Some(call.id.clone()),
            name: call.name.clone(),
        }));
    }
    if let Some(arguments) = optional_str(delta, "/function/arguments", context)? {
        call.arguments.push_str(arguments);
    }
    if streaming && call.started && call.streamed_arguments < call.arguments.len() {
        let delta = call.arguments[call.streamed_arguments..].to_owned();
        call.streamed_arguments = call.arguments.len();
        events.push(ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
            item_id: call.id.clone(),
            delta,
        }));
    }
    Ok(events)
}

fn optional_str<'a>(value: &'a Value, pointer: &str, context: &str) -> Result<Option<&'a str>> {
    match value.pointer(pointer) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => bail!("{context} returned non-string field {pointer}"),
    }
}

fn check_error(frame: &Value) -> Result<()> {
    if let Some(error) = frame.get("error").filter(|error| !error.is_null()) {
        bail!("Chat Completions error: {error}");
    }
    Ok(())
}

fn parse(frames: &[Value], model: &str) -> Result<ModelEventStream> {
    let mut output = Vec::new();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = BTreeMap::new();
    let mut usage = None;
    let mut finish_reason = None;
    for frame in frames {
        check_error(frame)?;
        if let Some(value) = frame.get("usage").filter(|value| !value.is_null()) {
            usage = Some(normalize_usage(value));
        }
        if let Some(value) = frame.pointer("/choices/0/finish_reason")
            && !value.is_null()
        {
            let reason = value
                .as_str()
                .context("Chat Completions returned a non-string finish_reason")?;
            anyhow::ensure!(
                finish_reason.replace(reason.to_owned()).is_none(),
                "Chat Completions returned multiple finish reasons"
            );
        }
        let Some(delta) = frame.pointer("/choices/0/delta") else {
            continue;
        };
        if let Some(value) = delta["content"].as_str() {
            text.push_str(value);
        }
        if let Some(value) = delta["refusal"].as_str() {
            text.push_str(value);
        }
        if let Some(value) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
        {
            reasoning.push_str(value);
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            apply_tool_delta(&mut calls, call, false)?;
        }
    }
    match finish_reason.as_deref() {
        Some("stop") => {
            anyhow::ensure!(
                calls.is_empty(),
                "Chat Completions finished with stop after returning tool calls"
            );
            // The schema permits an empty completion, but accepting one here can leave the
            // agent loop with no progress and cause it to repeat indefinitely.
            anyhow::ensure!(
                !text.is_empty(),
                "Chat Completions finished with stop without returning assistant text"
            );
        }
        Some("tool_calls") => anyhow::ensure!(
            !calls.is_empty(),
            "Chat Completions finished with tool_calls without returning a tool call"
        ),
        Some(reason) => bail!("Chat Completions ended with unsuccessful finish reason {reason}"),
        None => return Err(incomplete_stream("Chat Completions")),
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
    for (_, call) in calls {
        anyhow::ensure!(!call.id.is_empty(), "Chat Completions tool call omitted id");
        anyhow::ensure!(
            !call.name.is_empty(),
            "Chat Completions tool call omitted function name"
        );
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::ToolCall {
                name: call.name,
                arguments: serde_json::from_str(&call.arguments)
                    .context("Chat Completions returned invalid tool arguments")?,
                call_id: call.id,
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

    fn parse_error(frames: &[Value]) -> String {
        match parse(frames, "fixture-model") {
            Ok(_) => panic!("expected Chat Completions parsing to fail"),
            Err(error) => error.to_string(),
        }
    }

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
    async fn streams_and_parses_refusal_as_assistant_text() {
        let frames = vec![
            json!({"choices":[{"delta":{"refusal":"cannot comply"}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        ];
        let mut live = Live::default();

        assert!(matches!(
            live.decode(&frames[0]).unwrap().as_slice(),
            [ModelEvent::Update(ModelStreamEvent::AssistantDelta { content, phase })]
                if content == "cannot comply"
                    && *phase == atra_protocol::AssistantMessagePhase::Commentary
        ));

        let events = parse(&frames, "fixture-model")
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, phase })
            }) if content == "cannot comply"
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
    fn streams_tool_call_start_and_arguments() {
        let frames = include_str!("fixtures/chat_completions_tool.ndjson")
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<Value>, _>>()
            .unwrap();
        let mut live = Live::default();
        let mut starts = 0;
        let mut arguments = String::new();

        for frame in frames {
            for event in live.decode(&frame).unwrap() {
                match event {
                    ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
                        item_id,
                        call_id,
                        name,
                    }) => {
                        starts += 1;
                        assert_eq!(item_id, "call_fixture");
                        assert_eq!(call_id.as_deref(), Some("call_fixture"));
                        assert_eq!(name, "command");
                    }
                    ModelEvent::Update(ModelStreamEvent::ToolCallDelta { item_id, delta }) => {
                        assert_eq!(item_id, "call_fixture");
                        arguments.push_str(&delta);
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(starts, 1);
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).unwrap()["command"],
            "printf 'DEEPSEEK_TOOL'"
        );
    }

    #[test]
    fn buffers_tool_arguments_until_id_and_name_arrive() {
        let frames = [
            json!({"id":"chat-1","choices":[{"delta":{"tool_calls":[{
                "index":0,
                "function":{"arguments":"{\"value\":"}
            }]}}]}),
            json!({"id":"chat-1","choices":[{"delta":{"tool_calls":[{
                "index":0,
                "id":"call-1"
            }]}}]}),
            json!({"id":"chat-1","choices":[{"delta":{"role":"assistant","tool_calls":[{
                "index":0,
                "function":{"name":"command","arguments":"1}"}
            }]}}]}),
        ];
        let mut live = Live::default();
        let events = frames
            .iter()
            .flat_map(|frame| live.decode(frame).unwrap())
            .collect::<Vec<_>>();

        assert!(matches!(
            events.as_slice(),
            [
                ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
                    item_id,
                    call_id: Some(call_id),
                    name,
                }),
                ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
                    item_id: delta_item_id,
                    delta,
                }),
            ] if item_id == "call-1"
                && call_id == "call-1"
                && name == "command"
                && delta_item_id == "call-1"
                && delta == "{\"value\":1}"
        ));
    }

    #[test]
    fn discards_pending_tool_state_when_a_retry_gets_a_new_response_id() {
        let frames = [
            json!({"id":"chat-1","choices":[{"delta":{"tool_calls":[{
                "index":0,
                "id":"stale-call"
            }]}}]}),
            json!({"id":"chat-2","choices":[{"delta":{"tool_calls":[{
                "index":0,
                "id":"fresh-call",
                "function":{"name":"command","arguments":"{}"}
            }]}}]}),
        ];
        let mut live = Live::default();

        assert!(live.decode(&frames[0]).unwrap().is_empty());
        assert!(matches!(
            live.decode(&frames[1]).unwrap().as_slice(),
            [
                ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
                    item_id,
                    call_id: Some(call_id),
                    name,
                }),
                ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
                    item_id: delta_item_id,
                    delta,
                }),
            ] if item_id == "fresh-call"
                && call_id == "fresh-call"
                && name == "command"
                && delta_item_id == "fresh-call"
                && delta == "{}"
        ));
    }

    #[tokio::test]
    async fn preserves_parallel_tool_calls_by_index() {
        let frames = vec![
            json!({"choices":[{"delta":{"tool_calls":[
                {"index":1,"id":"call-2","function":{"name":"second","arguments":"{\"value\":"}},
                {"index":0,"id":"call-1","function":{"name":"first","arguments":"{\"value\":"}}
            ]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"1}"}},
                {"index":1,"function":{"arguments":"2}"}}
            ]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let events = parse(&frames, "fixture-model")
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let calls = events
            .into_iter()
            .filter_map(|event| match event.unwrap() {
                ModelEvent::OutputItemDone {
                    response:
                        Some(ModelResponse::ToolCall {
                            name, arguments, ..
                        }),
                } => Some((name, arguments["value"].as_u64().unwrap())),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            calls,
            vec![("first".to_owned(), 1), ("second".to_owned(), 2)]
        );
    }

    #[test]
    fn rejects_invalid_tool_json() {
        let frames = vec![
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"command","arguments":"{"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        assert_eq!(
            parse_error(&frames),
            "Chat Completions returned invalid tool arguments"
        );
    }

    #[test]
    fn rejects_tool_calls_without_an_id() {
        let frames = vec![
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {"name": "command", "arguments": "{}"}
                        }]
                    }
                }]
            }),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ];

        assert_eq!(
            parse_error(&frames),
            "Chat Completions tool call omitted id"
        );
    }

    #[test]
    fn rejects_unsuccessful_finish_reasons() {
        for reason in ["length", "content_filter", "unknown"] {
            let frames = vec![
                json!({"choices":[{"delta":{"content":"partial"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":reason}]}),
            ];

            let error = parse_error(&frames);
            assert!(
                error.contains(reason),
                "finish reason {reason} was not preserved in: {error}"
            );
        }
    }

    #[test]
    fn rejects_stop_without_assistant_text() {
        let frames = vec![
            json!({"choices":[{"delta":{"content":""},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        ];

        assert_eq!(
            parse_error(&frames),
            "Chat Completions finished with stop without returning assistant text"
        );
    }
}
