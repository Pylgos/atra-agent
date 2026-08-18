use std::{collections::VecDeque, fs, path::Path};

use anyhow::{Context, Result};
use async_trait::async_trait;
use atra_protocol::Model;
use futures_util::stream;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{
    DEFAULT_MODEL, ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ModelResponse,
    ModelSession, ProviderLoginStatus, ProviderOutput,
};
use crate::storage::Event;
use atra_protocol::{AssistantMessagePhase, ThreadEventData, ToolResultEvent};

pub(crate) struct FakeProvider {
    responses: Mutex<VecDeque<ModelResponse>>,
}

const PROVIDER_ID: &str = super::FAKE_PROVIDER;

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
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    async fn models(&self) -> Result<Vec<Model>> {
        Ok(vec![Model {
            provider: PROVIDER_ID.to_owned(),
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

    async fn login(&self, _credential: Option<String>) -> Result<ProviderLoginStatus> {
        Ok(ProviderLoginStatus::LoggedIn(None))
    }

    async fn login_status(&self) -> Result<ProviderLoginStatus> {
        Ok(ProviderLoginStatus::LoggedIn(None))
    }

    async fn reload_auth(&self) -> Result<()> {
        Ok(())
    }

    async fn logout(&self) -> Result<()> {
        Ok(())
    }

    async fn rate_limits(&self) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Array(Vec::new()))
    }

    async fn execute_tool(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }

    async fn start_turn(&self, _session_id: &str) -> Result<Box<dyn ModelSession + '_>> {
        Ok(Box::new(self))
    }

    fn context_tokens(&self, events: &[Event]) -> Result<usize> {
        Ok(super::text_tokens(&serde_json::to_string(events)?))
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

fn response_item(response: ModelResponse) -> Result<Value> {
    Ok(match response {
        ModelResponse::AssistantMessage { content, phase } => json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": content}],
            "phase": match phase {
                AssistantMessagePhase::Commentary => MessagePhase::Commentary,
                AssistantMessagePhase::FinalAnswer => MessagePhase::FinalAnswer,
            }
        }),
        ModelResponse::WebSearch { item } | ModelResponse::Reasoning { item } => {
            anyhow::ensure!(
                item.is_object(),
                "fake model script contains invalid output item"
            );
            item
        }
        ModelResponse::ToolCall {
            name,
            arguments,
            call_id,
        } => json!({
            "type": "function_call",
            "name": name,
            "arguments": arguments.to_string(),
            "call_id": call_id.expect("fake function tool call ID was assigned"),
        }),
        ModelResponse::CustomToolCall {
            item_id,
            name,
            input,
            call_id,
        } => json!({
            "type": "custom_tool_call",
            "id": item_id,
            "status": "completed",
            "call_id": call_id,
            "name": name,
            "input": match input {
                super::CustomToolInput::Text(input) => input.clone(),
                super::CustomToolInput::Arguments(arguments) => arguments.to_string(),
            },
        }),
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum MessagePhase {
    Commentary,
    FinalAnswer,
}
