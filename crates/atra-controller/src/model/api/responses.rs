use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream};
use reqwest::Client;
use serde_json::{Value, json};

use super::{
    function_tools, incomplete_stream, request_stream, required_nonempty_str, required_str,
    required_u64, value_text,
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
    let mut live = Live::new(model, replay_namespace);
    request_stream(
        attempt,
        "Responses",
        move |frame| live.decode(frame),
        move |frames, rate_limits| parse(&frames, rate_limits),
    )
}

pub(crate) async fn decode_server_compaction(response: reqwest::Response) -> Result<Value> {
    let mut frames = super::sse_stream(response, "Codex server-side compaction");
    let mut compaction = None;
    let mut completed = false;
    while let Some(frame) = frames.next().await {
        let frame = frame?;
        match required_str(&frame, "type", "Codex compaction event")? {
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

struct Live {
    model: String,
    replay_namespace: String,
    messages: BTreeMap<String, atra_protocol::AssistantMessagePhase>,
    calls: BTreeSet<String>,
    completed_items: BTreeMap<u64, Value>,
    next_output_index: u64,
    has_tool_call: bool,
}

impl Live {
    fn new(model: String, replay_namespace: String) -> Self {
        Self {
            model,
            replay_namespace,
            messages: BTreeMap::new(),
            calls: BTreeSet::new(),
            completed_items: BTreeMap::new(),
            next_output_index: 0,
            has_tool_call: false,
        }
    }

    fn decode(&mut self, frame: &Value) -> Result<Vec<ModelEvent>> {
        Ok(
            match required_str(frame, "type", "Responses stream event")? {
                "response.created" => {
                    self.messages.clear();
                    self.calls.clear();
                    self.completed_items.clear();
                    self.next_output_index = 0;
                    self.has_tool_call = false;
                    Vec::new()
                }
                "response.completed" => self.emit_completed_items(true)?,
                "response.output_item.added" => self
                    .item_added(&frame["item"])?
                    .into_iter()
                    .map(ModelEvent::Update)
                    .collect(),
                "response.output_item.done" => self.item_done(frame)?,
                "response.output_text.delta" | "response.refusal.delta" => {
                    let item_id =
                        required_nonempty_str(frame, "item_id", "Responses message delta event")?;
                    let phase = self.messages.get(item_id).copied().with_context(|| {
                        format!("Responses message delta referenced unknown message {item_id}")
                    })?;
                    vec![ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                        content: required_str(frame, "delta", "Responses message delta event")?
                            .to_owned(),
                        phase,
                    })]
                }
                "response.reasoning_summary_text.delta" => {
                    vec![ModelEvent::Update(ModelStreamEvent::ReasoningSummaryDelta(
                        required_str(frame, "delta", "Responses reasoning delta event")?.to_owned(),
                    ))]
                }
                "response.reasoning_summary_part.added" => {
                    vec![ModelEvent::Update(
                        ModelStreamEvent::ReasoningSummaryPartAdded,
                    )]
                }
                "response.function_call_arguments.delta"
                | "response.custom_tool_call_input.delta" => {
                    let item_id =
                        required_nonempty_str(frame, "item_id", "Responses tool call delta event")?;
                    anyhow::ensure!(
                        self.calls.contains(item_id),
                        "Responses tool call delta referenced unknown item {item_id}"
                    );
                    vec![ModelEvent::Update(ModelStreamEvent::ToolCallDelta {
                        item_id: item_id.to_owned(),
                        delta: required_str(frame, "delta", "Responses tool call delta event")?
                            .to_owned(),
                    })]
                }
                "response.web_search_call.in_progress"
                | "response.web_search_call.searching"
                | "response.web_search_call.completed" => Vec::new(),
                "error" | "response.failed" => bail!("Responses error: {frame}"),
                _ => Vec::new(),
            },
        )
    }

    fn item_added(&mut self, item: &Value) -> Result<Option<ModelStreamEvent>> {
        Ok(match required_str(item, "type", "Responses output item")? {
            "message" => {
                let item_id = required_nonempty_str(item, "id", "Responses message item")?;
                self.messages
                    .insert(item_id.to_owned(), live_message_phase(item)?);
                None
            }
            "function_call" | "custom_tool_call" => {
                let context = "Responses tool call item";
                let item_id = required_nonempty_str(item, "id", context)?.to_owned();
                let call_id = required_nonempty_str(item, "call_id", context)?.to_owned();
                let name = required_nonempty_str(item, "name", context)?.to_owned();
                self.has_tool_call = true;
                self.calls.insert(item_id.clone());
                Some(ModelStreamEvent::ToolCallStarted {
                    item_id,
                    call_id: Some(call_id),
                    name,
                })
            }
            "web_search_call" => Some(ModelStreamEvent::WebSearchUpdate {
                item_id: required_nonempty_str(item, "id", "Responses web_search_call item")?
                    .to_owned(),
                action: item.get("action").filter(|value| !value.is_null()).cloned(),
            }),
            _ => None,
        })
    }

    fn item_done(&mut self, frame: &Value) -> Result<Vec<ModelEvent>> {
        let index = required_u64(frame, "output_index", "Responses output_item.done event")?;
        anyhow::ensure!(
            index >= self.next_output_index
                && self
                    .completed_items
                    .insert(index, frame["item"].clone())
                    .is_none(),
            "Responses completed output item {index} more than once"
        );
        if matches!(
            frame.pointer("/item/type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call")
        ) {
            self.has_tool_call = true;
        }

        self.emit_completed_items(false)
    }

    fn emit_completed_items(&mut self, response_completed: bool) -> Result<Vec<ModelEvent>> {
        let default_phase = if self.has_tool_call {
            atra_protocol::AssistantMessagePhase::Commentary
        } else {
            atra_protocol::AssistantMessagePhase::FinalAnswer
        };
        let mut events = Vec::new();
        while let Some(item) = self.completed_items.get(&self.next_output_index) {
            let awaiting_later_tool_call = !response_completed
                && !self.has_tool_call
                && item["type"] == "message"
                && item.get("phase").is_none_or(Value::is_null);
            if awaiting_later_tool_call {
                break;
            }
            let item = self
                .completed_items
                .remove(&self.next_output_index)
                .expect("completed item exists");
            events.push(ModelEvent::OutputItemDone {
                response: response_from_item(
                    &item,
                    &self.model,
                    &self.replay_namespace,
                    default_phase,
                )?,
            });
            self.next_output_index += 1;
        }
        if response_completed {
            anyhow::ensure!(
                self.completed_items.is_empty(),
                "Responses completed with a missing output item before index {}",
                self.next_output_index
            );
        }
        Ok(events)
    }
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
    let message =
        |role: &str, content: Value, phase: Option<atra_protocol::AssistantMessagePhase>| {
            let mut value = json!({"type": "message", "role": role, "content": content});
            if let Some(phase) = phase {
                value["phase"] = Value::String(
                    match phase {
                        atra_protocol::AssistantMessagePhase::Commentary => "commentary",
                        atra_protocol::AssistantMessagePhase::FinalAnswer => "final_answer",
                    }
                    .to_owned(),
                );
            }
            value
        };
    let replay_key = super::super::compaction_replay_key(
        if matches!(profile, Profile::Codex) {
            "codex"
        } else {
            "responses"
        },
        request.model,
    );
    super::super::surface::derive(request.events, Some(&replay_key), request.kind)?
        .items
        .into_iter()
        .filter_map(|item| {
            let value = match item {
                Item::Message {
                    role, text, phase, ..
                } => Ok(match role {
                    Role::Developer => message(
                        "developer",
                        json!([{"type": "input_text", "text": text}]),
                        None,
                    ),
                    Role::User => {
                        message("user", json!([{"type": "input_text", "text": text}]), None)
                    }
                    Role::Assistant => message("assistant", Value::String(text), phase),
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
                } if matches!(profile, Profile::Codex) => {
                    let mut value = json!({
                        "type": "custom_tool_call",
                        "call_id": call_id,
                        "name": name,
                        "input": input,
                    });
                    if let Some(item_id) = item_id {
                        value["id"] = Value::String(item_id);
                    }
                    Ok(value)
                }
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

fn parse(frames: &[Value], rate_limits: Vec<Value>) -> Result<ModelEventStream> {
    let mut usage = None;
    let mut completed = false;

    for frame in frames {
        match required_str(frame, "type", "Responses stream event")? {
            "response.completed" => {
                completed = true;
                usage = frame.pointer("/response/usage").map(normalize_usage);
            }
            "error" | "response.failed" | "response.incomplete" => {
                bail!("Responses error: {frame}")
            }
            _ => {}
        }
    }
    if !completed {
        return Err(incomplete_stream("Responses"));
    }
    let output = [ModelEvent::Completed {
        token_usage: usage,
        rate_limits,
    }];
    Ok(Box::pin(stream::iter(output.into_iter().map(Ok))))
}

fn response_from_item(
    item: &Value,
    model: &str,
    replay_namespace: &str,
    default_phase: atra_protocol::AssistantMessagePhase,
) -> Result<Option<ModelResponse>> {
    Ok(
        match required_str(item, "type", "Responses completed output item")? {
            "reasoning" => Some(ModelResponse::Reasoning {
                summary: reasoning_summary(item)?,
                opaque: atra_protocol::OpaqueState {
                    replay_key: format!("{replay_namespace}/{model}/reasoning-v1"),
                    payload: item.clone(),
                },
            }),
            "message" => {
                let phase = item
                    .get("phase")
                    .filter(|phase| !phase.is_null())
                    .map(message_phase)
                    .transpose()?
                    .unwrap_or(default_phase);
                message_text(item)?
                    .map(|content| ModelResponse::AssistantMessage { content, phase })
            }
            "function_call" => {
                let context = "Responses completed function_call item";
                Some(ModelResponse::ToolCall {
                    name: required_nonempty_str(item, "name", context)?.to_owned(),
                    arguments: serde_json::from_str(required_str(item, "arguments", context)?)
                        .context("Responses returned invalid tool arguments")?,
                    call_id: required_nonempty_str(item, "call_id", context)?.to_owned(),
                })
            }
            "custom_tool_call" => {
                let context = "Responses completed custom_tool_call item";
                Some(ModelResponse::CustomToolCall {
                    item_id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned),
                    name: required_nonempty_str(item, "name", context)?.to_owned(),
                    input: required_str(item, "input", context)?.to_owned(),
                    call_id: required_nonempty_str(item, "call_id", context)?.to_owned(),
                })
            }
            "web_search_call" => Some(ModelResponse::WebSearch { item: item.clone() }),
            _ => None,
        },
    )
}

fn reasoning_summary(item: &Value) -> Result<String> {
    item["summary"]
        .as_array()
        .context("Responses reasoning item summary is not an array")?
        .iter()
        .map(|part| {
            anyhow::ensure!(
                required_str(part, "type", "Responses reasoning summary part")? == "summary_text",
                "Responses reasoning item contained unknown summary part {}",
                part["type"]
            );
            required_str(part, "text", "Responses reasoning summary part").map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("\n"))
}

fn message_text(item: &Value) -> Result<Option<String>> {
    let content = item["content"]
        .as_array()
        .context("Responses message item content is not an array")?;
    let mut text = String::new();
    for part in content {
        match required_str(part, "type", "Responses message content")? {
            "output_text" => {
                text.push_str(required_str(part, "text", "Responses output_text content")?)
            }
            "refusal" => text.push_str(required_str(part, "refusal", "Responses refusal content")?),
            other => bail!("Responses message contained unknown content {other}"),
        }
    }
    Ok((!text.is_empty()).then_some(text))
}

fn live_message_phase(item: &Value) -> Result<atra_protocol::AssistantMessagePhase> {
    match item.get("phase").filter(|phase| !phase.is_null()) {
        Some(phase) => message_phase(phase),
        None => Ok(atra_protocol::AssistantMessagePhase::Commentary),
    }
}

fn message_phase(value: &Value) -> Result<atra_protocol::AssistantMessagePhase> {
    match value.as_str() {
        Some("commentary") => Ok(atra_protocol::AssistantMessagePhase::Commentary),
        Some("final_answer") => Ok(atra_protocol::AssistantMessagePhase::FinalAnswer),
        Some(phase) => bail!("Responses message item returned unknown phase {phase}"),
        None => bail!("Responses message item phase is not a string"),
    }
}

fn normalize_usage(value: &Value) -> Value {
    json!({
        "input_tokens": value["input_tokens"],
        "cached_input_tokens": value.pointer("/input_tokens_details/cached_tokens"),
        "cache_write_input_tokens": value.pointer("/input_tokens_details/cache_write_tokens"),
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

    async fn decode_frames(
        frames: &[Value],
        model: &str,
        replay_namespace: &str,
    ) -> Result<Vec<ModelEvent>> {
        let mut live = Live::new(model.to_owned(), replay_namespace.to_owned());
        let mut events = Vec::new();
        for frame in frames {
            events.extend(live.decode(frame)?);
        }
        let mut finished = parse(frames, Vec::new())?;
        while let Some(event) = finished.next().await {
            events.push(event?);
        }
        Ok(events)
    }

    #[tokio::test]
    async fn preserves_completed_item_order_and_each_reasoning_item() {
        let frames = vec![
            json!({"type":"response.output_item.done","output_index":1,"item":{"id":"call-item","type":"function_call","call_id":"call-1","name":"command","arguments":"{\"command\":\"true\",\"runner\":\"sandbox\"}"}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"id":"rs-1","type":"reasoning","summary":[{"type":"summary_text","text":"first"},{"type":"summary_text","text":"second"}],"encrypted_content":"opaque-1"}}),
            json!({"type":"response.output_item.done","output_index":3,"item":{"id":"msg-1","type":"message","phase":"final_answer","content":[{"type":"output_text","text":"done"}]}}),
            json!({"type":"response.output_item.done","output_index":2,"item":{"id":"rs-2","type":"reasoning","summary":[],"encrypted_content":"opaque-2"}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}}),
        ];
        let events = decode_frames(&frames, "fixture-model", "responses")
            .await
            .unwrap();

        assert!(matches!(
            &events[0],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::Reasoning { summary, opaque })
            } if summary == "first\nsecond" && opaque.payload["encrypted_content"] == "opaque-1"
        ));
        assert!(matches!(
            &events[1],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::ToolCall { call_id, .. })
            } if call_id == "call-1"
        ));
        assert!(matches!(
            &events[2],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::Reasoning { summary, opaque })
            } if summary.is_empty() && opaque.payload["encrypted_content"] == "opaque-2"
        ));
        assert!(matches!(
            &events[3],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, phase })
            } if content == "done"
                && *phase == atra_protocol::AssistantMessagePhase::FinalAnswer
        ));
        assert!(matches!(events.last(), Some(ModelEvent::Completed { .. })));
    }

    #[tokio::test]
    async fn parses_scrubbed_live_tool_fixture() {
        let frames = include_str!("fixtures/responses_tool.ndjson")
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<Vec<Value>, _>>()
            .unwrap();
        let events = decode_frames(&frames, "gpt-5.6-luna", "responses")
            .await
            .unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::ToolCall {
                    call_id,
                    arguments,
                    ..
                })
            } if call_id == "call_fixture"
                && arguments["command"] == "printf 'LUNA_TOOL'"
        )));
        assert!(matches!(
            events.last(),
            Some(ModelEvent::Completed {
                token_usage: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn rejects_function_items_without_a_call_id() {
        let mut live = Live::new("fixture-model".to_owned(), "fixture".to_owned());
        assert!(
            live.decode(&json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type": "function_call", "id": "item-1", "name": "command", "arguments": "{}"}
            }))
            .is_err()
        );
    }

    #[test]
    fn completed_item_is_emitted_immediately() {
        let mut live = Live::new("fixture-model".to_owned(), "responses".to_owned());
        let events = live
            .decode(&json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "id":"msg-1",
                    "type":"message",
                    "phase":"final_answer",
                    "content":[{"type":"output_text","text":"done"}]
                }
            }))
            .unwrap();

        assert!(matches!(
            &events[0],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, .. })
            } if content == "done"
        ));
    }

    #[test]
    fn a_retried_response_discards_pending_items_from_the_previous_attempt() {
        let mut live = Live::new("fixture-model".to_owned(), "responses".to_owned());
        assert!(
            live.decode(&json!({
                "type":"response.output_item.done",
                "output_index":1,
                "item":{"type":"reasoning","summary":[]}
            }))
            .unwrap()
            .is_empty()
        );

        live.decode(&json!({"type":"response.created"})).unwrap();
        let events = live
            .decode(&json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"message",
                    "phase":"final_answer",
                    "content":[{"type":"output_text","text":"retried"}]
                }
            }))
            .unwrap();

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, .. })
            } if content == "retried"
        ));
    }

    #[test]
    fn live_output_uses_the_message_item_phase() {
        let mut live = Live::new("fixture-model".to_owned(), "codex".to_owned());
        assert!(
            live.decode(&json!({
                "type":"response.output_item.added",
                "item":{"id":"msg-1","type":"message","phase":"final_answer"}
            }))
            .unwrap()
            .is_empty()
        );
        let updates = live
            .decode(&json!({
                "type":"response.output_text.delta",
                "item_id":"msg-1",
                "delta":"done"
            }))
            .unwrap();
        assert!(matches!(
            &updates[0],
            ModelEvent::Update(ModelStreamEvent::AssistantDelta { content, phase })
                if content == "done"
                    && *phase == atra_protocol::AssistantMessagePhase::FinalAnswer
        ));
    }

    #[test]
    fn codex_messages_fall_back_when_phase_is_absent() {
        let mut live = Live::new("fixture-model".to_owned(), "codex".to_owned());
        assert!(
            live.decode(&json!({
                "type":"response.output_item.added",
                "item":{"id":"msg-1","type":"message"}
            }))
            .is_ok()
        );

        let updates = live
            .decode(&json!({
                "type":"response.output_text.delta",
                "item_id":"msg-1",
                "delta":"done"
            }))
            .unwrap();
        assert!(matches!(
            &updates[0],
            ModelEvent::Update(ModelStreamEvent::AssistantDelta { phase, .. })
                if *phase == atra_protocol::AssistantMessagePhase::Commentary
        ));

        assert!(
            live.decode(&json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "id":"msg-1",
                    "type":"message",
                    "content":[{"type":"output_text","text":"done"}]
                }
            }))
            .unwrap()
            .is_empty()
        );
        let events = live
            .decode(&json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}
            }))
            .unwrap();
        assert!(matches!(
            &events[0],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { phase, .. })
            } if *phase == atra_protocol::AssistantMessagePhase::FinalAnswer
        ));
    }

    #[test]
    fn standard_messages_fall_back_when_phase_is_absent() {
        let mut live = Live::new("fixture-model".to_owned(), "responses".to_owned());
        assert!(
            live.decode(&json!({
                "type":"response.output_item.added",
                "item":{"id":"msg-1","type":"message"}
            }))
            .is_ok()
        );

        let events = live
            .decode(&json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "id":"msg-1",
                    "type":"message",
                    "content":[{"type":"output_text","text":"done"}]
                }
            }))
            .unwrap();
        assert!(events.is_empty());

        let events = live
            .decode(&json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}
            }))
            .unwrap();
        assert!(matches!(
            &events[0],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { phase, .. })
            } if *phase == atra_protocol::AssistantMessagePhase::FinalAnswer
        ));
    }

    #[test]
    fn phase_less_message_before_a_tool_call_is_commentary() {
        let mut live = Live::new("fixture-model".to_owned(), "responses".to_owned());
        assert!(
            live.decode(&json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "id":"msg-1",
                    "type":"message",
                    "content":[{"type":"output_text","text":"working"}]
                }
            }))
            .unwrap()
            .is_empty()
        );
        live.decode(&json!({
            "type":"response.output_item.added",
            "output_index":1,
            "item":{
                "id":"call-item",
                "type":"function_call",
                "call_id":"call-1",
                "name":"command",
                "arguments":""
            }
        }))
        .unwrap();
        let events = live
            .decode(&json!({
                "type":"response.output_item.done",
                "output_index":1,
                "item":{
                    "id":"call-item",
                    "type":"function_call",
                    "call_id":"call-1",
                    "name":"command",
                    "arguments":"{}"
                }
            }))
            .unwrap();

        assert!(matches!(
            &events[0],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::AssistantMessage { content, phase })
            } if content == "working"
                && *phase == atra_protocol::AssistantMessagePhase::Commentary
        ));
        assert!(matches!(
            &events[1],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::ToolCall { call_id, .. })
            } if call_id == "call-1"
        ));
    }

    #[test]
    fn rejects_missing_required_protocol_fields() {
        let mut live = Live::new("fixture-model".to_owned(), "responses".to_owned());
        assert!(live.decode(&json!({})).is_err());
        assert!(
            live.decode(&json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{"id":"rs-1","type":"reasoning"}
            }))
            .is_err()
        );
    }

    #[test]
    fn web_search_status_events_do_not_clear_the_item_action() {
        let mut live = Live::new("fixture-model".to_owned(), "codex".to_owned());
        let events = live
            .decode(&json!({
                "type":"response.output_item.added",
                "item":{
                    "id":"ws-1",
                    "type":"web_search_call",
                    "action":{"type":"search","query":"atra"}
                }
            }))
            .unwrap();
        assert!(matches!(
            &events[0],
            ModelEvent::Update(ModelStreamEvent::WebSearchUpdate {
                item_id,
                action: Some(action),
            }) if item_id == "ws-1" && action["query"] == "atra"
        ));

        for event_type in [
            "response.web_search_call.in_progress",
            "response.web_search_call.searching",
            "response.web_search_call.completed",
        ] {
            assert!(
                live.decode(&json!({"type":event_type,"item_id":"ws-1"}))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn completed_custom_tool_call_may_omit_item_id() {
        let mut live = Live::new("fixture-model".to_owned(), "codex".to_owned());
        let events = live
            .decode(&json!({
                "type":"response.output_item.done",
                "output_index":0,
                "item":{
                    "type":"custom_tool_call",
                    "call_id":"call-1",
                    "name":"command",
                    "input":"*** Runner sandbox\ntrue"
                }
            }))
            .unwrap();
        assert!(matches!(
            &events[0],
            ModelEvent::OutputItemDone {
                response: Some(ModelResponse::CustomToolCall { item_id: None, .. })
            }
        ));
    }

    #[test]
    fn completed_response_rejects_a_gap_in_output_indexes() {
        let mut live = Live::new("fixture-model".to_owned(), "responses".to_owned());
        assert!(
            live.decode(&json!({
                "type":"response.output_item.done",
                "output_index":1,
                "item":{"id":"rs-1","type":"reasoning","summary":[]}
            }))
            .unwrap()
            .is_empty()
        );
        assert!(
            live.decode(&json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}
            }))
            .is_err()
        );
    }

    #[test]
    fn replays_matching_reasoning_items_without_changing_them() {
        use atra_protocol::{
            AssistantMessageEvent, AssistantMessagePhase, EventSequence, OpaqueState,
            ReasoningEvent, ThreadEventData,
        };

        let first = json!({
            "id":"rs-1",
            "type":"reasoning",
            "summary":[{"type":"summary_text","text":"first"}],
            "encrypted_content":"opaque-1"
        });
        let second = json!({
            "id":"rs-2",
            "type":"reasoning",
            "summary":[],
            "encrypted_content":"opaque-2"
        });
        let events = vec![
            crate::storage::Event {
                sequence: EventSequence(0),
                data: ThreadEventData::Reasoning(ReasoningEvent {
                    summary: "first".to_owned(),
                    opaque: Some(OpaqueState {
                        replay_key: "responses/fixture-model/reasoning-v1".to_owned(),
                        payload: first.clone(),
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
                        replay_key: "responses/fixture-model/reasoning-v1".to_owned(),
                        payload: second.clone(),
                    }),
                }),
            },
        ];
        let request = ModelRequest {
            kind: atra_protocol::ModelRequestKind::Response,
            model: "fixture-model",
            reasoning_effort: "high",
            instructions: "instructions",
            tools: &[],
            events: &events,
            prompt_cache_key: "cache",
        };

        assert_eq!(
            input(&request, Profile::Standard).unwrap(),
            vec![
                first,
                json!({
                    "type":"message",
                    "role":"assistant",
                    "phase":"commentary",
                    "content":"commentary"
                }),
                second,
            ]
        );
    }

    #[test]
    fn custom_tool_call_without_an_item_id_omits_the_id() {
        use atra_protocol::{EventSequence, ThreadEventData, ToolCallEvent};

        let events = vec![crate::storage::Event {
            sequence: EventSequence(0),
            data: ThreadEventData::ToolCall(ToolCallEvent::Custom {
                item_id: None,
                call_id: "call-1".to_owned(),
                name: "command".to_owned(),
                input: "true".to_owned(),
            }),
        }];
        let request = ModelRequest {
            kind: atra_protocol::ModelRequestKind::Response,
            model: "fixture-model",
            reasoning_effort: "high",
            instructions: "instructions",
            tools: &[],
            events: &events,
            prompt_cache_key: "cache",
        };

        let input = input(&request, Profile::Codex).unwrap();

        assert_eq!(
            input,
            vec![json!({
                "type": "custom_tool_call",
                "call_id": "call-1",
                "name": "command",
                "input": "true",
            })]
        );
    }

    #[test]
    fn server_compaction_appends_only_the_trigger() {
        let tools = crate::tools::model_tools(true);
        let events = Vec::new();
        let request = ModelRequest {
            kind: atra_protocol::ModelRequestKind::Response,
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
