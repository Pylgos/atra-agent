use std::{collections::VecDeque, fs, path::Path};

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use super::{ModelCompletion, ModelResponse};
use crate::storage::{Event, EventKind};

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
        if let ModelResponse::AssistantMessage { content } = &mut response
            && let Some(output) = events.iter().rev().find_map(|event| {
                (event.kind == EventKind::ToolResult)
                    .then(|| {
                        event.payload["result"]
                            .as_str()
                            .or_else(|| event.payload.pointer("/result/output")?.as_str())
                    })
                    .flatten()
            })
        {
            *content = content.replace("{{tool_output}}", output);
        }
        Ok(ModelCompletion {
            response,
            reasoning: Vec::new(),
            token_usage: None,
            rate_limits: Vec::new(),
        })
    }
}
