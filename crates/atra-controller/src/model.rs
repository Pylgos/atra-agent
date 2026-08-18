use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use atra_protocol::{
    ApprovalPolicy, AssistantMessagePhase, CommandTimerState, Model, Runner, RunnerOperationUpdate,
    SkillInvocationEvent,
};
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::storage::Event;

pub(crate) mod codex;
pub(crate) mod codex_auth;
mod fake;
pub(crate) mod ollama;

pub(crate) const CODEX_PROVIDER: &str = "codex";
pub(crate) const FAKE_PROVIDER: &str = "fake";
pub(crate) const OLLAMA_PROVIDER: &str = "ollama";
pub(crate) const DEFAULT_MODEL: &str = "gpt-5.6-sol";
pub(crate) const BASE_INSTRUCTIONS: &str = indoc::indoc! {r#"
    You are an expert coding assistant operating inside Atra, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files

    Commands execute on Atra Runners. The available Runners are provided in the conversation context. For each tool call, choose a suitable Runner with no more access than the operation requires.

    Guidelines:
    - Be concise in your responses.
    - Show file paths clearly when working with files.
    - For non-trivial work, put a todo annotation at the beginning of a commentary message or final answer when the todo state changes. Use `<todo>` and `</todo>` on separate lines with one or more `- [x]: completed`, `- [-]: in progress`, or `- [ ]: pending` lines between them. Omit the annotation when the todo state does not change.
    - Do not bypass or weaken Runner restrictions, sandbox boundaries, or Controller approval decisions.
    - Use subagents only when the user message, applicable AGENTS.md instructions, or an applicable skill explicitly instructs you to do so. Do not create subagents merely because delegation or parallelism may be useful.
    - When creating a subagent, omit `--model` and `--effort` so it inherits them from its parent unless the user message, applicable AGENTS.md instructions, or an applicable skill explicitly requires an override. Do not select a different model or reasoning effort merely because a task appears complex, important, or suited to a particular role.
    - The same rule applies to recursive delegation. A subagent may create child agents only when its thread context allows delegation and one of those instruction sources explicitly authorizes recursive delegation.
    - You are responsible for subagents you create. Before completing your turn, wait for required results and stop every descendant that is still running. Do not leave automated child turns running in the background."#};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelResponse {
    AssistantMessage {
        content: String,
        phase: AssistantMessagePhase,
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
        input: CustomToolInput,
        call_id: String,
    },
    Reasoning {
        item: serde_json::Value,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum CustomToolInput {
    Text(String),
    Arguments(serde_json::Value),
}

pub(crate) enum ModelStreamEvent {
    Retry {
        summary: String,
        current: u64,
        max: u64,
    },
    AssistantDelta {
        content: String,
        phase: AssistantMessagePhase,
    },
    ReasoningSummaryDelta(String),
    ReasoningSummaryPartAdded,
    WebSearchUpdate {
        item_id: String,
        action: Option<serde_json::Value>,
    },
    ToolCallStarted {
        item_id: String,
        call_id: Option<String>,
        name: String,
    },
    ToolCallDelta {
        item_id: String,
        delta: String,
    },
    RunnerOperationUpdate {
        call_id: String,
        operation_index: usize,
        runner: Option<String>,
        update: RunnerOperationUpdate,
    },
    RunnerOperationOutput {
        call_id: String,
        operation_index: usize,
        content: String,
        omitted_bytes: usize,
        timer: CommandTimerState,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProviderOutput {
    pub provider: String,
    pub data: serde_json::Value,
}

pub(crate) enum ModelEvent {
    Update(ModelStreamEvent),
    OutputItemDone {
        output: ProviderOutput,
        response: Option<ModelResponse>,
    },
    Completed {
        metadata: Option<ModelResponseMetadata>,
        token_usage: Option<serde_json::Value>,
        rate_limits: Vec<serde_json::Value>,
    },
}

pub(crate) struct ModelResponseMetadata {
    pub provider: String,
    pub response_id: String,
}

pub(crate) enum ProviderLoginStatus {
    LoggedIn(Option<String>),
    LoginRequired,
}

pub(crate) type ModelEventStream = BoxStream<'static, Result<ModelEvent>>;

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
    Function {
        name: &'static str,
        description: &'static str,
        parameters: serde_json::Value,
    },
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
    fn id(&self) -> &'static str;

    async fn models(&self) -> Result<Vec<Model>>;

    async fn login(&self, credential: Option<String>) -> Result<ProviderLoginStatus>;

    async fn login_status(&self) -> Result<ProviderLoginStatus>;

    async fn reload_auth(&self) -> Result<()>;

    async fn logout(&self) -> Result<()>;

    async fn rate_limits(&self) -> Result<serde_json::Value>;

    async fn execute_tool(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>>;

    async fn start_turn(&self, session_id: &str) -> Result<Box<dyn ModelSession + '_>>;

    fn context_tokens(&self, events: &[Event]) -> Result<usize>;
}

#[async_trait]
pub(crate) trait ModelSession: Send + Sync {
    async fn stream(&self, request: &ModelRequest<'_>) -> Result<ModelEventStream>;

    async fn compact(&self, request: &ModelRequest<'_>) -> Result<Option<ProviderOutput>>;
}

pub(crate) fn fake(path: &std::path::Path) -> Result<Arc<dyn ModelProvider>> {
    Ok(Arc::new(fake::FakeProvider::load(path)?))
}

pub(crate) async fn codex(auth_home: std::path::PathBuf) -> Arc<codex::CodexProvider> {
    Arc::new(codex::CodexProvider::new(auth_home).await)
}

pub(crate) fn ollama(auth_home: std::path::PathBuf) -> Arc<ollama::OllamaProvider> {
    Arc::new(ollama::OllamaProvider::new(auth_home))
}

pub(crate) fn text_tokens(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton()
        .encode_ordinary(text)
        .len()
}

pub(crate) fn format_skill_invocation(invocation: &SkillInvocationEvent) -> String {
    format!(
        "The user explicitly invoked the following skill for this request. Follow its \
         instructions and resolve relative references against the directory containing \
         {path}.\n\n<skill name=\"{name}\" path=\"{path}\">\n<instructions>\n{instructions}\
         \n</instructions>\n</skill>",
        name = invocation.name,
        path = invocation.path,
        instructions = invocation.instructions,
    )
}

pub(crate) fn format_runners(runners: &[Runner]) -> String {
    if runners.is_empty() {
        return "No Atra Runners are currently available.".to_owned();
    }
    let mut lines = vec!["Available Atra Runners:".to_owned()];
    lines.extend(runners.iter().map(|runner| {
        format!(
            "{}: {} (approval: {})",
            runner.name,
            runner
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            approval_name(runner.approval),
        )
    }));
    lines.push(String::new());
    lines.push(
        "Each Runner has an approval policy. Commands routed to a Runner with `ask` approval \
         require approval before execution; commands routed to a Runner with `allow` approval \
         execute without per-command approval. Approval is requested automatically when a command \
         is executed. If an approval request is denied, the tool call fails."
            .to_owned(),
    );
    lines.join("\n")
}

fn approval_name(approval: ApprovalPolicy) -> &'static str {
    match approval {
        ApprovalPolicy::Ask => "ask",
        ApprovalPolicy::Allow => "allow",
    }
}
