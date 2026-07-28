use std::path::{Path, PathBuf};

use crate::storage::Event;
use anyhow::{Context, Result};
use atra_protocol::{
    ApprovalId, AssistantMessagePhase, Model, RunnerOperationUpdate, ThreadEvent, ThreadId,
};
use codex_protocol::models::{ContentItem, MessagePhase, ResponseItem};
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
        #[serde(default)]
        phase: Option<AssistantMessagePhase>,
    },
    WebSearch {
        item: ResponseItem,
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
    Reasoning {
        item: ResponseItem,
    },
}

pub(crate) enum ModelStreamEvent {
    TurnStarted {
        thread_id: ThreadId,
    },
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
        approval_id: ApprovalId,
        thread_id: ThreadId,
        tool: String,
        arguments: serde_json::Value,
        operation_index: Option<usize>,
        operation_label: Option<String>,
    },
    RunnerOperationUpdate {
        call_id: String,
        operation_index: usize,
        update: RunnerOperationUpdate,
    },
    ThreadEvent(ThreadEvent),
}

pub(crate) struct ModelCompletion {
    pub output: Vec<ResponseItem>,
    pub response_id: Option<String>,
    pub token_usage: Option<TokenUsage>,
    pub rate_limits: Vec<RateLimitSnapshot>,
}

pub(crate) fn response_from_item(item: ResponseItem) -> Result<Option<ModelResponse>> {
    match item {
        ResponseItem::Message { content, phase, .. } => {
            let content = content
                .into_iter()
                .filter_map(|item| match item {
                    ContentItem::OutputText { text } => Some(text),
                    ContentItem::InputText { .. }
                    | ContentItem::InputImage { .. }
                    | ContentItem::InputAudio { .. } => None,
                })
                .collect::<String>();
            let phase = phase.map(|phase| match phase {
                MessagePhase::Commentary => AssistantMessagePhase::Commentary,
                MessagePhase::FinalAnswer => AssistantMessagePhase::FinalAnswer,
            });
            Ok((!content.is_empty()).then_some(ModelResponse::AssistantMessage { content, phase }))
        }
        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } => Ok(Some(ModelResponse::ToolCall {
            name,
            arguments: serde_json::from_str(&arguments)
                .context("Codex returned invalid tool arguments")?,
            call_id: Some(call_id),
        })),
        ResponseItem::CustomToolCall {
            id,
            name,
            input,
            call_id,
            ..
        } => Ok(Some(ModelResponse::CustomToolCall {
            item_id: id.map(String::from),
            name,
            input,
            call_id,
        })),
        item @ ResponseItem::WebSearchCall { .. } => Ok(Some(ModelResponse::WebSearch { item })),
        item @ ResponseItem::Reasoning { .. } => Ok(Some(ModelResponse::Reasoning { item })),
        _ => Ok(None),
    }
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

    pub(crate) fn context_tokens(&self, events: &[Event]) -> Result<usize> {
        codex::context_tokens(events)
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

pub(crate) fn text_tokens(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton()
        .encode_ordinary(text)
        .len()
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
