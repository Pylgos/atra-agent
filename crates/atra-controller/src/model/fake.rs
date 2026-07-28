use std::{collections::VecDeque, fs, path::Path};

use anyhow::{Context, Result};
use codex_protocol::ResponseItemId;
use codex_protocol::models::{ContentItem, MessagePhase, ResponseInputItem, ResponseItem};
use tokio::sync::Mutex;

use super::{ModelCompletion, ModelResponse};
use crate::storage::Event;
use atra_protocol::{AssistantMessagePhase, ThreadEventData, ToolResultEvent};

pub(crate) struct FakeProvider {
    responses: Mutex<VecDeque<ModelResponse>>,
}

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

    pub(super) async fn complete(&self, events: &[Event]) -> Result<ModelCompletion> {
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
        Ok(ModelCompletion {
            output: vec![response_item(response)],
            response_id: None,
            token_usage: None,
            rate_limits: Vec::new(),
        })
    }
}

fn response_item(response: ModelResponse) -> ResponseItem {
    match response {
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
        ModelResponse::WebSearch { item } | ModelResponse::Reasoning { item } => item,
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
    }
}
