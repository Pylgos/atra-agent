use std::{collections::VecDeque, fs, path::Path};

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use super::{ModelCompletion, ModelResponse};
use crate::storage::Event;
use atra_protocol::{ThreadEventData, ToolResultEvent};

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
        if let ModelResponse::AssistantMessage { content } = &mut response
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
        Ok(ModelCompletion {
            responses: vec![response],
            reasoning: Vec::new(),
            token_usage: None,
            rate_limits: Vec::new(),
        })
    }
}
