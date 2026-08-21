use std::{collections::VecDeque, fs, path::Path};

use anyhow::{Context, Result};
use async_trait::async_trait;
use atra_protocol::Model;
use futures_util::stream;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{
    DEFAULT_MODEL, ModelEvent, ModelEventStream, ModelRequest, ModelResponse, ProviderLoginStatus,
    ProviderRuntime,
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
        response_item(response.clone())?;
        Ok(Box::pin(stream::iter([
            Ok(ModelEvent::OutputItemDone {
                response: Some(response),
            }),
            Ok(ModelEvent::Completed {
                token_usage: None,
                rate_limits: Vec::new(),
            }),
        ])))
    }
}

#[async_trait]
impl ProviderRuntime for FakeProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn auth_method(&self) -> atra_protocol::ProviderAuthMethod {
        atra_protocol::ProviderAuthMethod::None
    }

    fn credential_source(&self) -> Option<atra_protocol::CredentialSource> {
        None
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
            tool_bindings: Vec::new(),
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
        _model: &str,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }

    async fn stream(
        &self,
        _session_id: &str,
        request: &ModelRequest<'_>,
    ) -> Result<ModelEventStream> {
        FakeProvider::stream(self, request.events).await
    }

    fn context_tokens(&self, events: &[Event]) -> Result<usize> {
        Ok(super::text_tokens(&serde_json::to_string(events)?))
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
        ModelResponse::WebSearch { item } => {
            anyhow::ensure!(
                item.is_object(),
                "fake model script contains invalid output item"
            );
            item
        }
        ModelResponse::Reasoning {
            summary, opaque, ..
        } => json!({"type": "reasoning", "summary": summary, "opaque": opaque}),
        ModelResponse::ToolCall {
            name,
            arguments,
            call_id,
        } => json!({
            "type": "function_call",
            "name": name,
            "arguments": arguments.to_string(),
            "call_id": call_id,
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
            "input": input,
        }),
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum MessagePhase {
    Commentary,
    FinalAnswer,
}
