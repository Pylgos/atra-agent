use std::{collections::BTreeMap, fmt, path::PathBuf};

use atra_patch::ApplyPatchResult;
use atra_store::TreeManifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod state;

pub use state::*;

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ThreadId(pub i64);

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct CheckpointId(pub i64);

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct EventSequence(pub i64);

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct InteractionId(pub u64);

#[derive(Clone, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ProcessId(pub String);

#[derive(Clone, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ProcessHandle(pub String);

impl fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for CheckpointId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for EventSequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for InteractionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for ProcessHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl AsRef<str> for ProcessId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ProcessHandle {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for ProcessId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<String> for ProcessHandle {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    Ask,
    Allow,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerOperationUpdate {
    CommandStarted {
        timer: CommandTimerState,
    },
    CommandOutput {
        content: String,
        omitted_bytes: usize,
        timer: CommandTimerState,
    },
    Completed {
        artifact: ToolArtifact,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTimerState {
    pub elapsed_ms: u64,
    pub remaining_ms: u64,
    pub paused: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProcessTiming {
    pub active_elapsed_ms: u64,
    pub paused: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ToolArtifact {
    CommandExecution(CommandExecutionArtifact),
    PatchOperations(ApplyPatchResult),
    RunnerOperation(RunnerOperationArtifact),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandExecutionArtifact {
    Started {
        runner: String,
    },
    Running {
        output: String,
        runner: String,
        full_output_path: PathBuf,
    },
    Finished {
        output: String,
        exit_code: Option<i32>,
        runner: String,
        full_output_path: PathBuf,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerOperationArtifact {
    pub operation: usize,
    pub runner: String,
    pub label: String,
    pub result: Value,
    pub artifacts: Vec<ToolArtifact>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadEvent {
    pub sequence: EventSequence,
    #[serde(flatten)]
    pub data: ThreadEventData,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ThreadEventData {
    WorkspaceInstructions(InstructionEvent),
    Skills(InstructionEvent),
    Runners(RunnersEvent),
    UserMessage(MessageEvent),
    AssistantMessage(AssistantMessageEvent),
    WebSearch(ItemEvent),
    ToolCall(ToolCallEvent),
    ToolResult(ToolResultEvent),
    FrozenBoundary(FrozenBoundaryEvent),
    Reasoning(ItemEvent),
    ModelOutput(ModelOutputEvent),
    Compaction(CompactionEvent),
    ModelRequest(ModelRequestEvent),
    TokenUsage(TokenUsageEvent),
    RateLimits(RateLimitsEvent),
}

impl ThreadEventData {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WorkspaceInstructions(_) => "workspace_instructions",
            Self::Skills(_) => "skills",
            Self::Runners(_) => "runners",
            Self::UserMessage(_) => "user_message",
            Self::AssistantMessage(_) => "assistant_message",
            Self::WebSearch(_) => "web_search",
            Self::ToolCall(_) => "tool_call",
            Self::ToolResult(_) => "tool_result",
            Self::FrozenBoundary(_) => "frozen_boundary",
            Self::Reasoning(_) => "reasoning",
            Self::ModelOutput(_) => "model_output",
            Self::Compaction(_) => "compaction",
            Self::ModelRequest(_) => "model_request",
            Self::TokenUsage(_) => "token_usage",
            Self::RateLimits(_) => "rate_limits",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "transition",
    content = "content",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum InstructionEvent {
    Initial(String),
    Replacement(String),
    Removal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "transition",
    content = "runners",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RunnersEvent {
    Initial(Vec<Runner>),
    Replacement(Vec<Runner>),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageEvent {
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantMessageEvent {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<AssistantMessagePhase>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoItem>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub step: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantMessagePhase {
    Commentary,
    FinalAnswer,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ItemEvent {
    pub item: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOutputEvent {
    pub request_sequence: EventSequence,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ToolCallEvent {
    Custom {
        #[serde(rename = "type")]
        call_type: CustomToolType,
        item_id: Option<String>,
        name: String,
        input: String,
        call_id: String,
    },
    Function {
        name: String,
        arguments: Value,
        call_id: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomToolType {
    Custom,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ToolResultEvent {
    Custom {
        #[serde(rename = "type")]
        call_type: CustomToolType,
        name: String,
        call_id: Option<String>,
        result: Value,
        artifacts: Vec<ToolArtifact>,
        #[serde(skip_serializing_if = "Option::is_none")]
        masked_result: Option<Value>,
    },
    Function {
        #[serde(rename = "type")]
        call_type: Option<CustomToolType>,
        name: String,
        call_id: Option<String>,
        result: Value,
        artifacts: Vec<ToolArtifact>,
        #[serde(skip_serializing_if = "Option::is_none")]
        masked_result: Option<Value>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenBoundaryEvent {
    pub through_sequence: EventSequence,
    pub masked_sequences: Vec<EventSequence>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionEvent {
    pub items: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequestEvent {
    pub kind: ModelRequestKind,
    pub context_window: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequestKind {
    Compaction,
    Response,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TokenUsageEvent {
    pub request_sequence: EventSequence,
    pub usage: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitsEvent {
    pub request_sequence: EventSequence,
    pub snapshots: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Thread {
    pub id: ThreadId,
    pub display_name: Option<String>,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadCheckpoint {
    pub id: CheckpointId,
    pub thread_id: ThreadId,
    pub created_at_ms: i64,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryTarget {
    Message {
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
    },
    Checkpoint {
        checkpoint_id: CheckpointId,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Runner {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub provider: String,
    pub id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub default_reasoning_effort: String,
    pub supported_reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub context_window: Option<i64>,
    #[serde(default)]
    pub auto_compact_token_limit: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessStatus {
    Running,
    Exited { exit_code: Option<i32> },
    Unavailable { message: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RunnerRequestEnvelope {
    pub request_id: u64,
    #[serde(flatten)]
    pub request: RunnerRequest,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RunnerResponseEnvelope {
    pub request_id: u64,
    #[serde(flatten)]
    pub response: RunnerResponse,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CommandEnvironment {
    pub set: BTreeMap<String, String>,
    pub prepend_path: Vec<String>,
    pub append_path: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SpawnedProcess {
    pub process_id: ProcessId,
    pub process_handle: ProcessHandle,
    pub command: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RunnerRequest {
    Initialize,
    PrepareTree {
        manifest: TreeManifest,
    },
    UploadObject {
        digest: String,
        executable: bool,
        blob: String,
    },
    StartCommand {
        command: String,
        environment: CommandEnvironment,
        process_id: ProcessId,
        process_prefix: String,
    },
    SpawnProcess {
        parent_process_handle: ProcessHandle,
        command: String,
        cwd: PathBuf,
        environment: CommandEnvironment,
        process_id: ProcessId,
    },
    ApplyPatch {
        process_handle: ProcessHandle,
        cwd: PathBuf,
        patch: String,
    },
    ReplaceText {
        process_handle: ProcessHandle,
        cwd: PathBuf,
        path: PathBuf,
        old: String,
        new: String,
        replace_all: bool,
    },
    WaitProcess {
        process_handle: ProcessHandle,
        active_timeout_ms: u64,
    },
    WaitChildProcess {
        waiting_process_handle: ProcessHandle,
        process_handle: ProcessHandle,
        timeout_ms: u64,
    },
    SubscribeProcess {
        process_handle: ProcessHandle,
    },
    StopProcess {
        process_handle: ProcessHandle,
    },
    InspectProcess {
        process_handle: ProcessHandle,
    },
    ProcessStatus {
        process_handle: ProcessHandle,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunnerResponse {
    Ready,
    MissingObjects {
        digests: Vec<String>,
    },
    TreeReady {
        digest: String,
        path: String,
    },
    ObjectStored,
    ProcessStarted {
        process_handle: ProcessHandle,
        timing: ProcessTiming,
    },
    ProcessRunning {
        process_handle: ProcessHandle,
        output: CommandOutput,
        patch_results: Vec<ApplyPatchResult>,
        spawned_processes: Vec<SpawnedProcess>,
        timing: ProcessTiming,
    },
    ProcessFinished {
        output: CommandOutput,
        exit_code: Option<i32>,
        patch_results: Vec<ApplyPatchResult>,
        spawned_processes: Vec<SpawnedProcess>,
    },
    ProcessStopped {
        output: CommandOutput,
    },
    ProcessInspected {
        process_status: ProcessStatus,
        output_tail: String,
        omitted_bytes: usize,
    },
    ProcessStatus {
        process_status: ProcessStatus,
    },
    PatchCompleted {
        result: ApplyPatchResult,
    },
    ReplaceCompleted {
        result: ApplyPatchResult,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CommandOutput {
    pub content: String,
    pub omitted_bytes: usize,
    pub full_output_path: PathBuf,
}
