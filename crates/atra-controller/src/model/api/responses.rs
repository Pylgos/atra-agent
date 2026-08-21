use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde_json::{Value, json};

use super::{
    function_tools, incomplete_stream, request_stream, required_nonempty_str, required_str,
    value_text,
};
use crate::model::{
    ModelEvent, ModelEventStream, ModelRequest, ModelResponse, ModelStreamEvent,
    surface::{Item, Role, ToolInput, ToolKind},
};

#[derive(Clone, Copy)]
pub(crate) enum Profile {
    Standard,
    Codex,
}

pub(crate) async fn stream(
    client: &Client,
    url: &str,
    api_key: &str,
    request: &ModelRequest<'_>,
    profile: Profile,
) -> Result<ModelEventStream> {
    let body = request_body(request, profile)?;
    let client = client.clone();
    let url = url.to_owned();
    let api_key = api_key.to_owned();
    let model = request.model.to_owned();
    Ok(decode(
        move || {
            let request = client.post(&url).bearer_auth(&api_key).json(&body);
            async move {
                Ok((
                    request.send().await.context("Responses request failed")?,
                    Vec::new(),
                ))
            }
        },
        model,
        "responses".to_owned(),
    ))
}

pub(crate) fn decode<A, Fut>(
    attempt: A,
    model: String,
    replay_namespace: String,
) -> ModelEventStream
where
    A: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(reqwest::Response, Vec<Value>)>> + Send + 'static,
{
    request_stream(attempt, "Responses", live, move |frames, rate_limits| {
        parse(&frames, &model, &replay_namespace, rate_limits)
    })
}

pub(crate) async fn decode_server_compaction(response: reqwest::Response) -> Result<Value> {
    let mut frames = super::sse_stream(response, "Codex server-side compaction");
    let mut compaction = None;
    let mut completed = false;
    while let Some(frame) = frames.next().await {
        let frame = frame?;
        match frame["type"].as_str().unwrap_or_default() {
            "response.output_item.done" if frame["item"]["type"] == "compaction" => {
                anyhow::ensure!(
                    compaction.is_none(),
                    "Codex returned multiple compaction items"
                );
                compaction = Some(frame["item"].clone());
            }
            "response.completed" => completed = true,
            "response.created" | "response.in_progress" | "response.output_item.added" | "ping" => {
            }
            "error" | "response.failed" => bail!("Codex compaction failed: {frame}"),
            other => bail!("unknown Codex compaction event {other}"),
        }
    }
    anyhow::ensure!(
        completed,
        "Codex compaction stream ended before response.completed"
    );
    compaction.context("Codex compaction response contained no compaction item")
}

fn live(frame: &Value) -> Result<Vec<ModelStreamEvent>> {
    Ok(match frame["type"].as_str().unwrap_or_default() {
        "response.output_text.delta" => vec![ModelStreamEvent::AssistantDelta {
            content: required_str(frame, "delta", "Responses output_text.delta event")?.to_owned(),
            phase: atra_protocol::AssistantMessagePhase::Commentary,
        }],
        "response.reasoning_summary_text.delta" => {
            vec![ModelStreamEvent::ReasoningSummaryDelta(
                required_str(frame, "delta", "Responses reasoning delta event")?.to_owned(),
            )]
        }
        "response.reasoning_summary_part.added" => {
            vec![ModelStreamEvent::ReasoningSummaryPartAdded]
        }
        "response.web_search_call.in_progress"
        | "response.web_search_call.searching"
        | "response.web_search_call.completed" => {
            vec![ModelStreamEvent::WebSearchUpdate {
                item_id: frame["item_id"]
                    .as_str()
                    .or_else(|| frame["id"].as_str())
                    .filter(|item_id| !item_id.is_empty())
                    .context("Responses web search update omitted item_id")?
                    .to_owned(),
                action: frame.get("action").cloned(),
            }]
        }
        "error" | "response.failed" => bail!("Responses error: {frame}"),
        _ => Vec::new(),
    })
}

pub(crate) fn request_body(request: &ModelRequest<'_>, profile: Profile) -> Result<Value> {
    let tools = response_tools(request.tools, profile);
    let mut body = json!({
        "model": request.model,
        "instructions": request.instructions,
        "input": input(request, profile)?,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": request.prompt_cache_key,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = Value::String("auto".to_owned());
        body["parallel_tool_calls"] = Value::Bool(true);
    }
    if request.reasoning_effort != "default" {
        body["reasoning"] = json!({"effort": request.reasoning_effort, "summary": "detailed"});
    }
    if matches!(profile, Profile::Codex) {
        body["text"] = json!({"verbosity": "low"});
        body["client_metadata"] = json!({
            "session_id": request.prompt_cache_key,
            "thread_id": request.prompt_cache_key,
        });
    }
    Ok(body)
}

pub(crate) fn server_compaction_body(request: &ModelRequest<'_>) -> Result<Value> {
    let mut body = request_body(request, Profile::Codex)?;
    body["input"]
        .as_array_mut()
        .context("Codex compaction input is not an array")?
        .push(json!({"type": "compaction_trigger"}));
    Ok(body)
}

fn response_tools(tools: &[crate::model::ModelTool], profile: Profile) -> Vec<Value> {
    let mut values = Vec::new();
    for tool in tools {
        match tool {
            crate::model::ModelTool::WebSearch if matches!(profile, Profile::Codex) => {
                values.push(json!({"type": "web_search"}));
            }
            crate::model::ModelTool::WebSearch => {
                for tool in function_tools(std::slice::from_ref(tool)) {
                    values.push(json!({
                        "type": "function",
                        "name": tool.pointer("/function/name"),
                        "description": tool.pointer("/function/description"),
                        "parameters": tool.pointer("/function/parameters"),
                        "strict": false,
                    }));
                }
            }
            crate::model::ModelTool::Tool {
                name,
                custom: Some(custom),
                ..
            } if matches!(profile, Profile::Codex) => {
                values.push(json!({
                    "type": "custom",
                    "name": name,
                    "description": custom.description,
                    "format": {
                        "type": "grammar",
                        "syntax": custom.format.syntax,
                        "definition": custom.format.definition,
                    }
                }));
            }
            tool => {
                for tool in function_tools(std::slice::from_ref(tool)) {
                    values.push(json!({
                        "type": "function",
                        "name": tool.pointer("/function/name"),
                        "description": tool.pointer("/function/description"),
                        "parameters": tool.pointer("/function/parameters"),
                        "strict": false,
                    }));
                }
            }
        }
    }
    values
}

fn input(request: &ModelRequest<'_>, profile: Profile) -> Result<Vec<Value>> {
    let message = |role: &str, kind: &str, text: String| json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]});
    let replay_key = format!(
        "{}/{}/compaction-v1",
        if matches!(profile, Profile::Codex) {
            "codex"
        } else {
            "responses"
        },
        request.model
    );
    super::super::surface::derive(request.events, Some(&replay_key))?
        .items
        .into_iter()
        .filter_map(|item| {
            let value = match item {
                Item::Message { role, text, .. } => Ok(match role {
                    Role::Developer => message("developer", "input_text", text),
                    Role::User => message("user", "input_text", text),
                    Role::Assistant => message("assistant", "output_text", text),
                }),
                Item::Reasoning { opaque, .. } => Ok(opaque
                    .filter(|opaque| {
                        opaque.replay_key
                            == format!(
                                "{}/{}/reasoning-v1",
                                if matches!(profile, Profile::Codex) {
                                    "codex"
                                } else {
                                    "responses"
                                },
                                request.model
                            )
                    })
                    .map(|opaque| opaque.payload)
                    .unwrap_or(Value::Null)),
                Item::ToolCall {
                    kind: ToolKind::Custom,
                    item_id,
                    call_id,
                    name,
                    input: ToolInput::Text(input),
                } if matches!(profile, Profile::Codex) => Ok(json!({
                    "type": "custom_tool_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": name,
                    "input": input,
                })),
                Item::ToolCall {
                    call_id,
                    name,
                    input,
                    ..
                } => {
                    let arguments = serde_json::to_string(&match input {
                        ToolInput::Json(value) => value,
                        ToolInput::Text(value) => json!({"input": value}),
                    });
                    arguments
                        .map(|arguments| {
                            json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments,
                            })
                        })
                        .map_err(Into::into)
                }
                Item::ToolResult {
                    kind: ToolKind::Custom,
                    call_id,
                    name,
                    output,
                } if matches!(profile, Profile::Codex) => Ok(json!({
                    "type": "custom_tool_call_output",
                    "call_id": call_id,
                    "name": name,
                    "output": value_text(&output),
                })),
                Item::ToolResult {
                    call_id, output, ..
                } => Ok(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": value_text(&output),
                })),
                Item::WebSearch(value) if matches!(profile, Profile::Codex) => Ok(value),
                Item::WebSearch(_) => Ok(Value::Null),
                Item::Opaque(state) => Ok(state.payload),
            };
            match value {
                Ok(Value::Null) => None,
                value => Some(value),
            }
        })
        .collect()
}

fn parse(
    frames: &[Value],
    model: &str,
    replay_namespace: &str,
    rate_limits: Vec<Value>,
) -> Result<ModelEventStream> {
    let mut output = Vec::new();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_payload = None;
    let mut calls: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    let mut custom_calls: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    let mut usage = None;
    let mut completed = false;
    for frame in frames {
        match frame["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                let value = required_str(frame, "delta", "Responses output_text.delta event")?;
                text.push_str(value);
                output.push(Ok(ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                    content: value.to_owned(),
                    phase: atra_protocol::AssistantMessagePhase::Commentary,
                })));
            }
            "response.reasoning_summary_text.delta" => {
                let value = required_str(frame, "delta", "Responses reasoning delta event")?;
                reasoning.push_str(value);
                output.push(Ok(ModelEvent::Update(
                    ModelStreamEvent::ReasoningSummaryDelta(value.to_owned()),
                )));
            }
            "response.reasoning_summary_part.added" => {
                output.push(Ok(ModelEvent::Update(
                    ModelStreamEvent::ReasoningSummaryPartAdded,
                )));
            }
            "response.output_item.added" => {
                let item = &frame["item"];
                if item["type"] == "function_call" {
                    let id =
                        required_nonempty_str(item, "call_id", "Responses function_call item")?
                            .to_owned();
                    let name = required_nonempty_str(item, "name", "Responses function_call item")?
                        .to_owned();
                    let item_id =
                        required_nonempty_str(item, "id", "Responses function_call item")?
                            .to_owned();
                    calls.insert(id.clone(), (item_id.clone(), name.clone(), String::new()));
                    output.push(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
                        item_id,
                        call_id: Some(id),
                        name,
                    })));
                } else if item["type"] == "custom_tool_call" {
                    let call_id =
                        required_nonempty_str(item, "call_id", "Responses custom_tool_call item")?
                            .to_owned();
                    let item_id =
                        required_nonempty_str(item, "id", "Responses custom_tool_call item")?
                            .to_owned();
                    let name =
                        required_nonempty_str(item, "name", "Responses custom_tool_call item")?
                            .to_owned();
                    custom_calls.insert(
                        call_id.clone(),
                        (item_id.clone(), name.clone(), String::new()),
                    );
                    output.push(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallStarted {
                        item_id,
                        call_id: Some(call_id),
                        name,
                    })));
                }
            }
            "response.output_item.done" => {
                let item = &frame["item"];
                if item["type"] == "web_search_call" {
                    output.push(Ok(ModelEvent::OutputItemDone {
                        response: Some(ModelResponse::WebSearch { item: item.clone() }),
                    }));
                } else if item["type"] == "reasoning" {
                    reasoning_payload = Some(item.clone());
                } else if item["type"] == "function_call" {
                    let call_id = required_nonempty_str(
                        item,
                        "call_id",
                        "Responses completed function_call item",
                    )?;
                    let arguments =
                        required_str(item, "arguments", "Responses completed function_call item")?;
                    let call = calls.get_mut(call_id).with_context(|| {
                        format!("Responses completed unknown function call {call_id}")
                    })?;
                    call.2 = arguments.to_owned();
                } else if item["type"] == "custom_tool_call" {
                    let call_id = required_nonempty_str(
                        item,
                        "call_id",
                        "Responses completed custom_tool_call item",
                    )?;
                    let input =
                        required_str(item, "input", "Responses completed custom_tool_call item")?;
                    let call = custom_calls.get_mut(call_id).with_context(|| {
                        format!("Responses completed unknown custom tool call {call_id}")
                    })?;
                    call.2 = input.to_owned();
                }
            }
            "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed" => {
                output.push(Ok(ModelEvent::Update(ModelStreamEvent::WebSearchUpdate {
                    item_id: frame["item_id"]
                        .as_str()
                        .or_else(|| frame["id"].as_str())
                        .filter(|item_id| !item_id.is_empty())
                        .context("Responses web search event omitted item_id")?
                        .to_owned(),
                    action: frame.get("action").cloned(),
                })));
            }
            "response.function_call_arguments.delta" => {
                let item_id = required_nonempty_str(
                    frame,
                    "item_id",
                    "Responses function_call_arguments.delta event",
                )?;
                let value = required_str(
                    frame,
                    "delta",
                    "Responses function_call_arguments.delta event",
                )?;
                let call = if let Some(call_id) = frame["call_id"]
                    .as_str()
                    .filter(|call_id| !call_id.is_empty())
                {
                    calls.get_mut(call_id)
                } else {
                    calls
                        .values_mut()
                        .find(|(known_item_id, _, _)| known_item_id == item_id)
                }
                .context(
                    "Responses function_call_arguments.delta referenced an unknown function call",
                )?;
                call.2.push_str(value);
                output.push(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
                    item_id: item_id.to_owned(),
                    delta: value.to_owned(),
                })));
            }
            "response.custom_tool_call_input.delta" => {
                let delta = required_str(
                    frame,
                    "delta",
                    "Responses custom_tool_call_input.delta event",
                )?;
                let call = if let Some(call_id) = frame
                    .get("call_id")
                    .and_then(|value| value.as_str())
                    .filter(|call_id| !call_id.is_empty())
                {
                    custom_calls.get_mut(call_id)
                } else {
                    let item_id = required_nonempty_str(
                        frame,
                        "item_id",
                        "Responses custom_tool_call_input.delta event",
                    )?;
                    custom_calls
                        .values_mut()
                        .find(|(known_item_id, _, _)| known_item_id == item_id)
                }
                .with_context(|| {
                    format!(
                        "Responses custom_tool_call_input.delta referenced an unknown custom tool call: {frame}"
                    )
                })?;
                call.2.push_str(delta);
                output.push(Ok(ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
                    item_id: call.0.clone(),
                    delta: delta.to_owned(),
                })));
            }
            "response.completed" => {
                completed = true;
                usage = frame.pointer("/response/usage").map(normalize_usage);
            }
            "response.created"
            | "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.done"
            | "ping" => {}
            "error" | "response.failed" => bail!("Responses error: {frame}"),
            other => bail!("unknown Responses event {other}"),
        }
    }
    if !completed {
        return Err(incomplete_stream("Responses"));
    }
    if !reasoning.is_empty() {
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::Reasoning {
                summary: reasoning.clone(),
                opaque: atra_protocol::OpaqueState {
                    replay_key: format!("{replay_namespace}/{model}/reasoning-v1"),
                    payload: reasoning_payload
                        .unwrap_or_else(|| json!({"type": "reasoning", "summary": reasoning})),
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
    for (call_id, (_, name, arguments)) in calls {
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::ToolCall {
                name,
                arguments: serde_json::from_str(&arguments)
                    .context("Responses returned invalid tool arguments")?,
                call_id,
            }),
        }));
    }
    for (call_id, (item_id, name, input)) in custom_calls {
        output.push(Ok(ModelEvent::OutputItemDone {
            response: Some(ModelResponse::CustomToolCall {
                item_id: Some(item_id),
                name,
                input,
                call_id,
            }),
        }));
    }
    output.push(Ok(ModelEvent::Completed {
        token_usage: usage,
        rate_limits,
    }));
    Ok(Box::pin(stream::iter(output)))
}

fn normalize_usage(value: &Value) -> Value {
    json!({
        "input_tokens": value["input_tokens"],
        "cached_input_tokens": value.pointer("/input_tokens_details/cached_tokens"),
        "output_tokens": value["output_tokens"],
        "reasoning_output_tokens": value.pointer("/output_tokens_details/reasoning_tokens"),
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
            json!({"type":"response.reasoning_summary_part.added"}),
            json!({"type":"response.reasoning_summary_text.delta","delta":"think"}),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"opaque"}}),
            json!({"type":"response.output_text.delta","delta":"done"}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}}),
        ];
        let events = parse(&frames, "fixture-model", "responses", Vec::new())
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::OutputItemDone {
                response: Some(ModelResponse::Reasoning { summary, opaque })
            }) if summary == "think" && opaque.payload["encrypted_content"] == "opaque"
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
        let frames = include_str!("fixtures/responses_tool.ndjson")
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<Value>, _>>()
            .unwrap();
        let events = parse(&frames, "gpt-5.6-luna", "responses", Vec::new())
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
                && arguments["command"] == "printf 'LUNA_TOOL'"
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
    fn rejects_function_items_without_a_call_id() {
        assert!(
            parse(
                &[json!({
                    "type": "response.output_item.added",
                    "item": {"type": "function_call", "id": "item-1", "name": "command"}
                })],
                "fixture-model",
                "fixture",
                Vec::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_stream_event() {
        assert!(
            parse(
                &[json!({"type":"response.new_event"})],
                "fixture-model",
                "responses",
                Vec::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn server_compaction_appends_only_the_trigger() {
        let tools = crate::tools::model_tools(true);
        let events = Vec::new();
        let request = ModelRequest {
            model: "gpt-5.2",
            reasoning_effort: "high",
            instructions: "instructions",
            tools: &tools,
            events: &events,
            prompt_cache_key: "cache",
        };
        let normal = request_body(&request, Profile::Codex).unwrap();
        let compaction = server_compaction_body(&request).unwrap();
        let normal_input = normal["input"].as_array().unwrap();
        let compact_input = compaction["input"].as_array().unwrap();

        assert_eq!(&compact_input[..normal_input.len()], normal_input);
        assert_eq!(compact_input.last().unwrap()["type"], "compaction_trigger");
        for field in [
            "model",
            "instructions",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
            "prompt_cache_key",
        ] {
            assert_eq!(normal[field], compaction[field], "{field}");
        }
    }
}
