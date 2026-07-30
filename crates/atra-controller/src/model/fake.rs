use std::{collections::VecDeque, fs, path::Path};

use anyhow::{Context, Result};
use async_trait::async_trait;
use atra_protocol::Model;
use codex_protocol::ResponseItemId;
use codex_protocol::models::{ContentItem, MessagePhase, ResponseInputItem, ResponseItem};
use futures_util::stream;
use tokio::sync::Mutex;

use super::{
    DEFAULT_MODEL, ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ModelResponse,
    ModelSession, ProviderOutput,
};
use crate::storage::Event;
use atra_protocol::{AssistantMessagePhase, ThreadEventData, ToolResultEvent};

pub(crate) struct FakeProvider {
    responses: Mutex<VecDeque<ModelResponse>>,
}

const PROVIDER_ID: &str = "fake";

impl FakeProvider {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let script = fs::read(path)
            .with_context(|| format!("failed to read fake model script {}", path.display()))?;
        let responses = serde_json::from_slice(&script)
            .with_context(|| format!("failed to decode fake model script {}", path.display()))?;
        Ok(Self {
            responses: Mutex::new(responses),
        })
    }

    pub(super) async fn stream(&self, events: &[Event]) -> Result<ModelEventStream> {
        let mut response = self
            .responses
            .lock()
            .await
            .pop_front()
            .context("fake model script has no response remaining")?;
        let masked_sequences = crate::storage::latest_frozen_boundary(events)
            .map(|boundary| boundary.masked_sequences)
            .unwrap_or_default();
        if let ModelResponse::AssistantMessage { content, .. } = &mut response
            && let Some(output) = events.iter().rev().find_map(|event| {
                let ThreadEventData::ToolResult(result) = &event.data else {
                    return None;
                };
                let (result, masked_result) = match result {
                    ToolResultEvent::Custom {
                        result,
                        masked_result,
                        ..
                    }
                    | ToolResultEvent::Function {
                        result,
                        masked_result,
                        ..
                    } => (result, masked_result),
                };
                let projected = if masked_sequences.contains(&event.sequence) {
                    masked_result.as_ref().unwrap_or(result)
                } else {
                    result
                };
                projected
                    .as_str()
                    .or_else(|| projected.pointer("/output")?.as_str())
            })
        {
            *content = content.replace("{{tool_output}}", output);
        }
        if let ModelResponse::ToolCall { call_id, .. } = &mut response
            && call_id.is_none()
        {
            *call_id = Some(format!(
                "fake_call_{}",
                atra_id::generate().replace(' ', "_")
            ));
        }
        let output = response_item(response.clone())?;
        let output = ProviderOutput {
            provider: PROVIDER_ID.to_owned(),
            data: serde_json::to_value([output]).context("failed to encode fake model output")?,
        };
        Ok(Box::pin(stream::iter([
            Ok(ModelEvent::OutputItemDone {
                output,
                response: Some(response),
            }),
            Ok(ModelEvent::Completed {
                metadata: None,
                token_usage: None,
                rate_limits: Vec::new(),
            }),
        ])))
    }
}

#[async_trait]
impl ModelProvider for FakeProvider {
    async fn models(&self) -> Result<Vec<Model>> {
        Ok(vec![Model {
            id: DEFAULT_MODEL.to_owned(),
            display_name: DEFAULT_MODEL.to_owned(),
            description: None,
            default_reasoning_effort: "medium".to_owned(),
            supported_reasoning_efforts: ["low", "medium", "high", "xhigh"]
                .map(str::to_owned)
                .to_vec(),
            context_window: None,
            auto_compact_token_limit: None,
        }])
    }

    async fn start_turn(&self, _session_id: &str) -> Result<Box<dyn ModelSession + '_>> {
        Ok(Box::new(self))
    }

    fn completion_snapshot(&self, request: &ModelRequest<'_>) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "provider": PROVIDER_ID,
            "model": request.model,
            "reasoning_effort": request.reasoning_effort,
            "instructions": request.instructions,
            "tools": request.tools.len(),
            "events": request.events,
        }))
    }

    fn context_tokens(&self, events: &[Event]) -> Result<usize> {
        Ok(super::text_tokens(&serde_json::to_string(events)?))
    }

    fn compaction_snapshot(&self, request: &ModelRequest<'_>) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "provider": PROVIDER_ID,
            "kind": "compaction",
            "model": request.model,
            "reasoning_effort": request.reasoning_effort,
            "instructions": request.instructions,
            "tools": request.tools.len(),
            "events": request.events,
        }))
    }
}

#[async_trait]
impl ModelSession for &FakeProvider {
    async fn stream(&self, request: &ModelRequest<'_>) -> Result<ModelEventStream> {
        FakeProvider::stream(self, request.events).await
    }

    async fn compact(&self, _request: &ModelRequest<'_>) -> Result<Option<ProviderOutput>> {
        Ok(None)
    }
}

fn response_item(response: ModelResponse) -> Result<ResponseItem> {
    Ok(match response {
        ModelResponse::AssistantMessage { content, phase } => {
            ResponseItem::from(ResponseInputItem::Message {
                role: "assistant".to_owned(),
                content: vec![ContentItem::OutputText { text: content }],
                phase: phase.map(|phase| match phase {
                    AssistantMessagePhase::Commentary => MessagePhase::Commentary,
                    AssistantMessagePhase::FinalAnswer => MessagePhase::FinalAnswer,
                }),
            })
        }
        ModelResponse::WebSearch { item } | ModelResponse::Reasoning { item } => {
            serde_json::from_value(item)
                .context("fake model script contains invalid output item")?
        }
        ModelResponse::ToolCall {
            name,
            arguments,
            call_id,
        } => ResponseItem::FunctionCall {
            id: None,
            name,
            namespace: None,
            arguments: arguments.to_string(),
            call_id: call_id.expect("fake function tool call ID was assigned"),
            internal_chat_message_metadata_passthrough: None,
        },
        ModelResponse::CustomToolCall {
            item_id,
            name,
            input,
            call_id,
        } => ResponseItem::CustomToolCall {
            id: item_id.map(ResponseItemId::from_server),
            status: Some("completed".to_owned()),
            call_id,
            name,
            namespace: None,
            input,
            internal_chat_message_metadata_passthrough: None,
        },
    })
}
