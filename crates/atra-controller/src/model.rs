use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use atra_protocol::{
    ApprovalId, AssistantMessagePhase, Model, RunnerOperationUpdate, ThreadEvent, ThreadId,
};
use serde::{Deserialize, Serialize};

use crate::storage::Event;

pub(crate) mod codex;
mod fake;

pub(crate) const DEFAULT_MODEL: &str = "gpt-5.6-sol";
pub(crate) const BASE_INSTRUCTIONS: &str = indoc::indoc! {r#"
    You are an expert coding assistant operating inside Atra, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files

    Commands execute on Atra Runners. The available Runners are provided in the conversation context. For each tool call, choose a suitable Runner with no more access than the operation requires.

    Guidelines:
    - Be concise in your responses.
    - Show file paths clearly when working with files.
    - For non-trivial work, put a todo annotation at the beginning of a commentary message or final answer when the todo state changes. Use `<todo>` and `</todo>` on separate lines with one or more `- [x]: completed`, `- [-]: in progress`, or `- [ ]: pending` lines between them. Omit the annotation when the todo state does not change.
    - Do not bypass or weaken Runner restrictions, sandbox boundaries, or Controller approval decisions."#};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelResponse {
    AssistantMessage {
        content: String,
        #[serde(default)]
        phase: Option<AssistantMessagePhase>,
    },
    WebSearch {
        item: serde_json::Value,
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
        item: serde_json::Value,
    },
}

pub(crate) enum ModelStreamEvent {
    TurnStarted {
        thread_id: ThreadId,
    },
    Retry {
        current: u64,
        max: u64,
    },
    AssistantDelta(String),
    ReasoningSummaryDelta(String),
    ReasoningSummaryPartAdded,
    WebSearchUpdate {
        item_id: String,
        action: Option<serde_json::Value>,
    },
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

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProviderOutput {
    pub provider: String,
    pub data: serde_json::Value,
}

pub(crate) struct ModelCompletion {
    pub output: ProviderOutput,
    pub responses: Vec<ModelResponse>,
    pub response_id: Option<String>,
    pub token_usage: Option<serde_json::Value>,
    pub rate_limits: Vec<serde_json::Value>,
}

pub(crate) struct ModelRequest<'a> {
    pub model: &'a str,
    pub reasoning_effort: &'a str,
    pub instructions: &'a str,
    pub tools: &'a [ModelTool],
    pub events: &'a [Event],
    pub prompt_cache_key: &'a str,
}

pub(crate) enum ModelTool {
    WebSearch,
    Custom {
        name: &'static str,
        description: &'static str,
        format: ModelToolFormat,
    },
}

pub(crate) struct ModelToolFormat {
    pub syntax: &'static str,
    pub definition: &'static str,
}

#[async_trait]
pub(crate) trait ModelProvider: Send + Sync {
    async fn models(&self) -> Result<Vec<Model>>;

    async fn start_turn(&self, session_id: &str) -> Result<Box<dyn ModelSession + '_>>;

    fn completion_snapshot(&self, request: &ModelRequest<'_>) -> Result<serde_json::Value>;

    fn context_tokens(&self, events: &[Event]) -> Result<usize>;

    fn compaction_snapshot(&self, request: &ModelRequest<'_>) -> Result<serde_json::Value>;
}

#[async_trait]
pub(crate) trait ModelSession: Send + Sync {
    async fn complete(
        &self,
        request: &ModelRequest<'_>,
        updates: Option<&tokio::sync::mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ModelCompletion>;

    async fn compact(&self, request: &ModelRequest<'_>) -> Result<Option<ProviderOutput>>;
}

pub(crate) fn fake(path: &std::path::Path) -> Result<Arc<dyn ModelProvider>> {
    Ok(Arc::new(fake::FakeProvider::load(path)?))
}

pub(crate) async fn codex(auth_home: std::path::PathBuf) -> Arc<codex::CodexProvider> {
    Arc::new(codex::CodexProvider::new(auth_home).await)
}

pub(crate) fn text_tokens(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton()
        .encode_ordinary(text)
        .len()
}
