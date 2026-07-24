use std::path::{Path, PathBuf};

use crate::storage::Event;
use anyhow::Result;
use atra_protocol::{Model, ThreadEvent};
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use serde::Deserialize;

use self::{codex::CodexProvider, fake::FakeProvider};

mod codex;
mod fake;

pub(crate) const DEFAULT_MODEL: &str = "gpt-5.6-sol";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelResponse {
    AssistantMessage {
        content: String,
    },
    ToolCall {
        name: String,
        arguments: serde_json::Value,
        #[serde(default)]
        call_id: Option<String>,
    },
    CustomToolCall {
        item_id: Option<String>,
        name: String,
        input: String,
        call_id: String,
    },
}

pub(crate) enum ModelStreamEvent {
    AssistantDelta(String),
    ToolCallStarted { item_id: String, name: String },
    ToolCallDelta { item_id: String, delta: String },
    ThreadEvent(ThreadEvent),
}

pub(crate) struct ModelCompletion {
    pub response: ModelResponse,
    pub reasoning: Vec<ResponseItem>,
    pub token_usage: Option<TokenUsage>,
}

pub(crate) enum Provider {
    Fake(FakeProvider),
    Codex(CodexProvider),
}

impl Provider {
    pub(crate) fn fake(path: &Path) -> Result<Self> {
        Ok(Self::Fake(FakeProvider::load(path)?))
    }

    pub(crate) async fn codex(auth_home: PathBuf) -> Self {
        Self::Codex(CodexProvider::new(auth_home).await)
    }

    pub(crate) async fn login_status(&self) -> Option<Option<String>> {
        match self {
            Self::Fake(_) => Some(None),
            Self::Codex(provider) => provider.login_status().await,
        }
    }

    pub(crate) async fn reload_auth(&self) {
        if let Self::Codex(provider) = self {
            provider.reload_auth().await;
        }
    }

    pub(crate) async fn models(&self) -> Result<Vec<Model>> {
        match self {
            Self::Fake(_) => Ok(vec![Model {
                id: DEFAULT_MODEL.to_owned(),
                display_name: DEFAULT_MODEL.to_owned(),
                description: None,
                default_reasoning_effort: "medium".to_owned(),
                supported_reasoning_efforts: ["low", "medium", "high", "xhigh"]
                    .map(str::to_owned)
                    .to_vec(),
                context_window: None,
                auto_compact_token_limit: None,
            }]),
            Self::Codex(provider) => provider.models().await,
        }
    }

    pub(crate) async fn complete(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        updates: Option<&tokio::sync::mpsc::UnboundedSender<ModelStreamEvent>>,
        prompt_cache_key: &str,
    ) -> Result<ModelCompletion> {
        match self {
            Self::Fake(provider) => provider.complete(events).await,
            Self::Codex(provider) => {
                provider
                    .complete(model, reasoning_effort, events, updates, prompt_cache_key)
                    .await
            }
        }
    }

    pub(crate) async fn compact(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<Vec<ResponseItem>> {
        match self {
            Self::Fake(_) => Ok(Vec::new()),
            Self::Codex(provider) => {
                provider
                    .compact(model, reasoning_effort, events, prompt_cache_key)
                    .await
            }
        }
    }
}
