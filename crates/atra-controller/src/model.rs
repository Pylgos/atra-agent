use std::path::{Path, PathBuf};

use crate::storage::Event;
use anyhow::Result;
use atra_protocol::{Model, ThreadEvent};
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::{RateLimitSnapshot, TokenUsage};
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
    ReasoningSummaryDelta(String),
    ReasoningSummaryPartAdded,
    ToolCallStarted {
        item_id: String,
        name: String,
    },
    ToolCallDelta {
        item_id: String,
        delta: String,
    },
    ApprovalRequired {
        approval_id: u64,
        thread_id: i64,
        tool: String,
        arguments: serde_json::Value,
    },
    ThreadEvent(ThreadEvent),
}

pub(crate) struct ModelCompletion {
    pub responses: Vec<ModelResponse>,
    pub reasoning: Vec<ResponseItem>,
    pub token_usage: Option<TokenUsage>,
    pub rate_limits: Vec<RateLimitSnapshot>,
}

pub(crate) enum Provider {
    Fake(FakeProvider),
    Codex(CodexProvider),
}

pub(crate) enum TurnSession<'a> {
    Fake(&'a FakeProvider),
    Codex(codex::CodexTurn),
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

    pub(crate) async fn logout(&self) -> Result<()> {
        if let Self::Codex(provider) = self {
            provider.logout().await?;
        }
        Ok(())
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

    pub(crate) async fn start_turn(&self, session_id: &str) -> Result<TurnSession<'_>> {
        match self {
            Self::Fake(provider) => Ok(TurnSession::Fake(provider)),
            Self::Codex(provider) => Ok(TurnSession::Codex(
                provider.start_turn(session_id.to_owned()).await?,
            )),
        }
    }

    pub(crate) fn completion_snapshot(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<serde_json::Value> {
        match self {
            Self::Fake(_) => Ok(serde_json::json!({
                "provider": "fake",
                "model": model,
                "reasoning_effort": reasoning_effort,
                "events": events,
            })),
            Self::Codex(provider) => {
                provider.completion_snapshot(model, reasoning_effort, events, prompt_cache_key)
            }
        }
    }

    pub(crate) fn compaction_snapshot(
        &self,
        model: &str,
        reasoning_effort: &str,
        events: &[Event],
        prompt_cache_key: &str,
    ) -> Result<serde_json::Value> {
        match self {
            Self::Fake(_) => Ok(serde_json::json!({
                "provider": "fake",
                "kind": "compaction",
                "model": model,
                "reasoning_effort": reasoning_effort,
                "events": events,
            })),
            Self::Codex(provider) => {
                provider.compaction_snapshot(model, reasoning_effort, events, prompt_cache_key)
            }
        }
    }
}

impl TurnSession<'_> {
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
            Self::Codex(session) => {
                session
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
            Self::Codex(session) => {
                session
                    .compact(model, reasoning_effort, events, prompt_cache_key)
                    .await
            }
        }
    }
}
