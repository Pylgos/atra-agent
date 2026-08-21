use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use atra_protocol::{
    ApprovalPolicy, AssistantMessagePhase, CommandTimerState, CredentialSource, Model, OpaqueState,
    ProviderAuthMethod, Runner, RunnerOperationUpdate, SkillInvocationEvent,
};
use futures_util::stream::BoxStream;
use serde::Deserialize;

use crate::storage::Event;

mod api;
mod api_key_auth;
pub(crate) mod codex;
pub(crate) mod codex_auth;
mod fake;
pub(crate) mod ollama;
mod opencode_go;
mod registry;
mod surface;
mod tool_binding;

pub(crate) use registry::ProviderRegistry;

pub(crate) const CODEX_PROVIDER: &str = "codex";
pub(crate) const FAKE_PROVIDER: &str = "fake";
pub(crate) const OLLAMA_PROVIDER: &str = "ollama";
pub(crate) const OPENCODE_GO_PROVIDER: &str = "opencode-go";
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
        call_id: String,
    },
    CustomToolCall {
        item_id: Option<String>,
        name: String,
        input: String,
        call_id: String,
    },
    Reasoning {
        summary: String,
        opaque: OpaqueState,
    },
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
        runner: String,
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

pub(crate) enum ModelEvent {
    Update(ModelStreamEvent),
    OutputItemDone {
        response: Option<ModelResponse>,
    },
    Completed {
        token_usage: Option<serde_json::Value>,
        rate_limits: Vec<serde_json::Value>,
    },
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
    Tool {
        name: &'static str,
        json: ModelJsonToolInterface,
        custom: Option<ModelCustomToolInterface>,
    },
}

pub(crate) struct ModelJsonToolInterface {
    pub description: String,
    pub parameters: serde_json::Value,
}

pub(crate) struct ModelCustomToolInterface {
    pub description: String,
    pub format: ModelToolFormat,
}

pub(crate) struct ModelToolFormat {
    pub syntax: &'static str,
    pub definition: &'static str,
}

#[async_trait]
pub(crate) trait ProviderRuntime: Send + Sync {
    fn id(&self) -> &'static str;
    fn auth_method(&self) -> ProviderAuthMethod;
    fn credential_source(&self) -> Option<CredentialSource>;

    async fn models(&self) -> Result<Vec<Model>>;

    async fn login(&self, credential: Option<String>) -> Result<ProviderLoginStatus>;

    async fn login_status(&self) -> Result<ProviderLoginStatus>;

    async fn reload_auth(&self) -> Result<()>;

    async fn logout(&self) -> Result<()>;

    async fn rate_limits(&self) -> Result<serde_json::Value>;

    async fn execute_tool(
        &self,
        model: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>>;

    async fn stream(
        &self,
        session_id: &str,
        request: &ModelRequest<'_>,
    ) -> Result<ModelEventStream>;

    async fn server_compact(
        &self,
        _session_id: &str,
        _request: &ModelRequest<'_>,
    ) -> Result<Option<OpaqueState>> {
        Ok(None)
    }

    fn context_tokens(&self, events: &[Event]) -> Result<usize>;
}

pub(crate) struct Provider {
    runtime: Arc<dyn ProviderRuntime>,
}

impl Provider {
    pub(crate) fn new(runtime: impl ProviderRuntime + 'static) -> Arc<Self> {
        Self::from_runtime(Arc::new(runtime))
    }

    pub(crate) fn from_runtime(runtime: Arc<dyn ProviderRuntime>) -> Arc<Self> {
        Arc::new(Self { runtime })
    }

    pub(crate) fn id(&self) -> &'static str {
        self.runtime.id()
    }

    pub(crate) fn auth_method(&self) -> ProviderAuthMethod {
        self.runtime.auth_method()
    }

    pub(crate) fn credential_source(&self) -> Option<CredentialSource> {
        self.runtime.credential_source()
    }

    pub(crate) async fn models(&self) -> Result<Vec<Model>> {
        self.runtime.models().await
    }

    pub(crate) async fn login(&self, credential: Option<String>) -> Result<ProviderLoginStatus> {
        self.runtime.login(credential).await
    }

    pub(crate) async fn login_status(&self) -> Result<ProviderLoginStatus> {
        self.runtime.login_status().await
    }

    pub(crate) async fn reload_auth(&self) -> Result<()> {
        self.runtime.reload_auth().await
    }

    pub(crate) async fn logout(&self) -> Result<()> {
        self.runtime.logout().await
    }

    pub(crate) async fn rate_limits(&self) -> Result<serde_json::Value> {
        self.runtime.rate_limits().await
    }

    pub(crate) async fn execute_tool(
        &self,
        model: &str,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>> {
        self.runtime.execute_tool(model, name, arguments).await
    }

    pub(crate) async fn stream(
        &self,
        session_id: &str,
        request: &ModelRequest<'_>,
    ) -> Result<ModelEventStream> {
        self.runtime.stream(session_id, request).await
    }

    pub(crate) async fn server_compact(
        &self,
        session_id: &str,
        request: &ModelRequest<'_>,
    ) -> Result<Option<OpaqueState>> {
        self.runtime.server_compact(session_id, request).await
    }

    pub(crate) fn context_tokens(&self, events: &[Event]) -> Result<usize> {
        self.runtime.context_tokens(events)
    }
}

pub(crate) fn fake(path: &std::path::Path) -> Result<Arc<Provider>> {
    Ok(Provider::new(fake::FakeProvider::load(path)?))
}

pub(crate) async fn codex(auth_home: std::path::PathBuf) -> Arc<Provider> {
    Provider::new(codex::CodexProvider::new(auth_home).await)
}

pub(crate) fn ollama(auth_home: std::path::PathBuf) -> Arc<Provider> {
    Provider::new(ollama::OllamaProvider::new(auth_home))
}

pub(crate) fn opencode_go(auth_home: std::path::PathBuf) -> Arc<Provider> {
    Provider::new(opencode_go::OpenCodeGoProvider::new(auth_home))
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
