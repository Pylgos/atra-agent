use std::{collections::VecDeque, fs, path::Path};

use anyhow::{Context, Result};

use super::ModelResponse;
use crate::storage::{Event, EventKind};

pub(crate) struct FakeProvider {
    responses: VecDeque<ModelResponse>,
}

impl FakeProvider {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let script = fs::read(path)
            .with_context(|| format!("failed to read fake model script {}", path.display()))?;
        let responses = serde_json::from_slice(&script)
            .with_context(|| format!("failed to decode fake model script {}", path.display()))?;
        Ok(Self { responses })
    }

    pub(super) fn complete(&mut self, events: &[Event]) -> Result<ModelResponse> {
        let mut response = self
            .responses
            .pop_front()
            .context("fake model script has no response remaining")?;
        if let ModelResponse::AssistantMessage { content } = &mut response
            && let Some(output) = events.iter().rev().find_map(|event| {
                (event.kind == EventKind::ToolResult)
                    .then(|| event.payload.pointer("/result/output")?.as_str())
                    .flatten()
            })
        {
            *content = content.replace("{{tool_output}}", output);
        }
        Ok(response)
    }
}
