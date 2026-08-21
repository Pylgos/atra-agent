pub(super) mod chat_completions;
pub(super) mod messages;
pub(super) mod ollama;
pub(super) mod responses;

use anyhow::{Context, Result};
use futures_util::{StreamExt, stream, stream::BoxStream};
use reqwest::Response;
use serde_json::{Value, json};

use super::{
    ModelEvent, ModelEventStream, ModelRequest, ModelStreamEvent, ModelTool,
    surface::{Item, Role, ToolInput},
};

pub(super) fn function_tools(tools: &[ModelTool]) -> Vec<Value> {
    let mut values = Vec::new();
    for tool in tools {
        match tool {
            ModelTool::WebSearch => {
                values.push(function_tool(
                    "web_search",
                    "Search the web for current information.",
                    crate::tools::web_search_parameters(),
                ));
                values.push(function_tool(
                    "web_fetch",
                    "Fetch a public web page and return readable Markdown.",
                    crate::tools::web_fetch_parameters(),
                ));
            }
            ModelTool::Tool { name, json, .. } => values.push(function_tool(
                name,
                &json.description,
                json.parameters.clone(),
            )),
        }
    }
    values
}

pub(super) fn request_stream<A, Fut, M, F, G>(
    mut attempt: A,
    label: &'static str,
    mut live: F,
    finish: G,
) -> ModelEventStream
where
    A: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(Response, M)>> + Send + 'static,
    M: Send + 'static,
    F: FnMut(&Value) -> Result<Vec<ModelEvent>> + Send + 'static,
    G: Fn(Vec<Value>, M) -> Result<ModelEventStream> + Send + 'static,
{
    let (sender, receiver) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        let mut retries = 0;
        'attempts: loop {
            let (response, metadata) = match attempt().await {
                Ok(attempt)
                    if !(attempt.0.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || attempt.0.status().is_server_error()) =>
                {
                    attempt
                }
                Ok((response, _)) if retries < 5 => {
                    drop(response);
                    retries += 1;
                    retry_delay(retries).await;
                    continue;
                }
                Ok(attempt) => attempt,
                Err(error) if retries < 5 && retryable_request_error(&error) => {
                    retries += 1;
                    retry_delay(retries).await;
                    continue;
                }
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            };
            if retries > 0
                && sender
                    .send(Ok(ModelEvent::Update(ModelStreamEvent::Retry {
                        summary: "request retried before output".to_owned(),
                        current: retries,
                        max: 5,
                    })))
                    .await
                    .is_err()
            {
                return;
            }
            let mut frames = Vec::new();
            let mut canonical_output = false;
            let mut incoming = sse_stream(response, label);
            while let Some(frame) = incoming.next().await {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error)
                        if !canonical_output
                            && retries < 5
                            && error.downcast_ref::<StreamTransportError>().is_some() =>
                    {
                        retries += 1;
                        retry_delay(retries).await;
                        continue 'attempts;
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                };
                match live(&frame) {
                    Ok(events) => {
                        canonical_output |= !events.is_empty();
                        for event in events {
                            if sender.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                }
                frames.push(frame);
            }
            let mut completed = match finish(frames, metadata) {
                Ok(completed) => completed,
                Err(error)
                    if !canonical_output
                        && retries < 5
                        && error.downcast_ref::<IncompleteStream>().is_some() =>
                {
                    retries += 1;
                    retry_delay(retries).await;
                    continue;
                }
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            };
            while let Some(event) = completed.next().await {
                match event {
                    Ok(ModelEvent::Update(_)) => {}
                    event => {
                        if sender.send(event).await.is_err() {
                            return;
                        }
                    }
                }
            }
            return;
        }
    });
    stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|event| (event, receiver))
    })
    .boxed()
}

fn retryable_request_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|error| error.is_timeout() || error.is_connect() || error.is_body())
}

async fn retry_delay(retries: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(
        250_u64.saturating_mul(1 << retries.min(5)),
    ))
    .await;
}

#[derive(Debug)]
struct StreamTransportError(String);

impl std::fmt::Display for StreamTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for StreamTransportError {}

#[derive(Debug)]
struct IncompleteStream(&'static str);

impl std::fmt::Display for IncompleteStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} stream ended before completion", self.0)
    }
}

impl std::error::Error for IncompleteStream {}

pub(super) fn incomplete_stream(label: &'static str) -> anyhow::Error {
    anyhow::Error::new(IncompleteStream(label))
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

pub(super) fn chat_messages(request: &ModelRequest<'_>) -> Result<Vec<Value>> {
    let mut messages = vec![json!({
        "role": "system",
        "content": request.instructions,
    })];
    for item in super::surface::derive(request.events, None)?.items {
        match item {
            Item::Message { role, text, .. } => messages.push(json!({
                "role": match role {
                    Role::Developer => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                "content": text,
            })),
            Item::ToolCall {
                call_id,
                name,
                input,
                ..
            } => {
                let call = json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": match input {
                            ToolInput::Json(value) => serde_json::to_string(&value)?,
                            ToolInput::Text(value) => serde_json::to_string(
                                &json!({"input": value})
                            )?,
                        }
                    }
                });
                if let Some(last) = messages.last_mut()
                    && last["role"] == "assistant"
                    && last["content"].is_null()
                    && last["tool_calls"].is_array()
                {
                    last["tool_calls"]
                        .as_array_mut()
                        .expect("tool_calls is an array")
                        .push(call);
                } else {
                    messages.push(json!({
                        "role": "assistant",
                        "content": Value::Null,
                        "tool_calls": [call],
                    }));
                }
            }
            Item::ToolResult {
                call_id, output, ..
            } => messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": value_text(&output),
            })),
            Item::Reasoning { .. } | Item::WebSearch(_) | Item::Opaque(_) => {}
        }
    }
    Ok(messages)
}

pub(super) fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

pub(super) fn required_str<'a>(value: &'a Value, field: &str, context: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .with_context(|| format!("{context} omitted string field {field}"))
}

pub(super) fn required_nonempty_str<'a>(
    value: &'a Value,
    field: &str,
    context: &str,
) -> Result<&'a str> {
    let value = required_str(value, field, context)?;
    anyhow::ensure!(!value.is_empty(), "{context} returned an empty {field}");
    Ok(value)
}

pub(super) fn required_u64(value: &Value, field: &str, context: &str) -> Result<u64> {
    value[field]
        .as_u64()
        .with_context(|| format!("{context} omitted integer field {field}"))
}

pub(super) fn sse_stream(
    response: Response,
    label: &'static str,
) -> BoxStream<'static, Result<Value>> {
    let (sender, receiver) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        let response = if response.status().is_success() {
            response
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let _ = sender
                .send(Err(anyhow::anyhow!(
                    "{label} request failed with {status}: {body}"
                )))
                .await;
            return;
        };
        let mut bytes = response.bytes_stream();
        let mut buffer = Vec::new();
        while let Some(chunk) = bytes.next().await {
            match chunk {
                Ok(chunk) => buffer.extend_from_slice(&chunk),
                Err(error) => {
                    let _ = sender
                        .send(Err(anyhow::Error::new(StreamTransportError(format!(
                            "{label} stream failed: {error}"
                        )))))
                        .await;
                    return;
                }
            }
            while let Some(end) = frame_end(&buffer) {
                let frame = buffer.drain(..end).collect::<Vec<_>>();
                let delimiter = if buffer.starts_with(b"\r\n\r\n") {
                    4
                } else {
                    2
                };
                buffer.drain(..delimiter);
                match decode_sse_frame(&frame, label) {
                    Ok(Some(value)) => {
                        if sender.send(Ok(value)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                }
            }
        }
        if !buffer.iter().all(u8::is_ascii_whitespace) {
            match decode_sse_frame(&buffer, label) {
                Ok(Some(value)) => {
                    let _ = sender.send(Ok(value)).await;
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                }
            }
        }
    });
    stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|event| (event, receiver))
    })
    .boxed()
}

fn frame_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .or_else(|| buffer.windows(2).position(|window| window == b"\n\n"))
}

fn decode_sse_frame(frame: &[u8], label: &str) -> Result<Option<Value>> {
    let frame =
        std::str::from_utf8(frame).with_context(|| format!("{label} stream was not UTF-8"))?;
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&data).with_context(|| {
        format!("invalid {label} SSE event: {data}")
    })?))
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use futures_util::{StreamExt, stream};

    fn response(body: impl Into<reqwest::Body>) -> Response {
        http::Response::builder()
            .status(200)
            .body(body.into())
            .unwrap()
            .into()
    }

    fn completed() -> Result<ModelEventStream> {
        Ok(stream::iter([Ok(ModelEvent::Completed {
            token_usage: None,
            rate_limits: Vec::new(),
        })])
        .boxed())
    }

    #[tokio::test]
    async fn retries_a_stream_disconnect_before_canonical_output() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let stream = request_stream(
            {
                let attempts = Arc::clone(&attempts);
                move || {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    async move {
                        let response = if attempt == 0 {
                            response(reqwest::Body::wrap_stream(stream::once(async {
                                Err::<Vec<u8>, _>(io::Error::new(
                                    io::ErrorKind::ConnectionReset,
                                    "disconnected",
                                ))
                            })))
                        } else {
                            response("data: {\"type\":\"delta\",\"text\":\"ok\"}\n\n")
                        };
                        Ok((response, ()))
                    }
                }
            },
            "fixture",
            |frame| match frame["type"].as_str() {
                Some("delta") => Ok(vec![ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                    content: frame["text"].as_str().unwrap().to_owned(),
                    phase: atra_protocol::AssistantMessagePhase::Commentary,
                })]),
                _ => Ok(Vec::new()),
            },
            |_, ()| completed(),
        )
        .collect::<Vec<_>>()
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(stream.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::Update(ModelStreamEvent::Retry {
                current: 1,
                ..
            }))
        )));
        assert!(stream.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                content,
                ..
            })) if content == "ok"
        )));
        assert!(matches!(
            stream.last(),
            Some(Ok(ModelEvent::Completed { .. }))
        ));
    }

    #[tokio::test]
    async fn retries_a_cleanly_ended_incomplete_stream_before_output() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let stream = request_stream(
            {
                let attempts = Arc::clone(&attempts);
                move || {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    async move {
                        let body = if attempt == 0 {
                            "data: {\"type\":\"ping\"}\n\n"
                        } else {
                            "data: {\"type\":\"completed\"}\n\n"
                        };
                        Ok((response(body), ()))
                    }
                }
            },
            "fixture",
            |_| Ok(Vec::new()),
            |frames, ()| {
                if frames.iter().any(|frame| frame["type"] == "completed") {
                    completed()
                } else {
                    Err(incomplete_stream("fixture"))
                }
            },
        )
        .collect::<Vec<_>>()
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(matches!(
            stream.last(),
            Some(Ok(ModelEvent::Completed { .. }))
        ));
    }

    #[tokio::test]
    async fn does_not_retry_after_canonical_output() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let body = reqwest::Body::wrap_stream(stream::iter([
            Ok::<_, io::Error>(b"data: {\"type\":\"delta\",\"text\":\"partial\"}\n\n".to_vec()),
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "disconnected",
            )),
        ]));
        let stream = request_stream(
            {
                let attempts = Arc::clone(&attempts);
                let mut body = Some(body);
                move || {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    let response = response(body.take().expect("only one request is expected"));
                    async move { Ok((response, ())) }
                }
            },
            "fixture",
            |frame| {
                Ok(vec![ModelEvent::Update(ModelStreamEvent::AssistantDelta {
                    content: frame["text"].as_str().unwrap().to_owned(),
                    phase: atra_protocol::AssistantMessagePhase::Commentary,
                })])
            },
            |_, ()| completed(),
        )
        .collect::<Vec<_>>()
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(stream.iter().any(Result::is_err));
    }
}
