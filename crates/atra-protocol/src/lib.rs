use std::{collections::BTreeMap, fmt, path::PathBuf};

use atra_patch::ApplyPatchResult;
use atra_store::TreeManifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
pub struct ApprovalId(pub u64);

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

impl fmt::Display for ApprovalId {
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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CommandMode {
    Foreground { timeout_ms: Option<u64> },
    Background,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum ControllerRequest {
    Shutdown,
    Turn(TurnRequest),
    Unary(UnaryRequest),
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum TurnRequest {
    ThreadSend {
        thread_id: ThreadId,
        message: String,
    },
    ThreadContinue {
        thread_id: ThreadId,
    },
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum UnaryRequest {
    Status,
    ThreadCreate {
        display_name: Option<String>,
    },
    ThreadList,
    ModelList,
    ThreadRename {
        thread_id: ThreadId,
        display_name: String,
    },
    ThreadSetModel {
        thread_id: ThreadId,
        model: String,
        reasoning_effort: String,
    },
    ThreadEvents {
        thread_id: ThreadId,
    },
    ThreadCheckpointCreate {
        thread_id: ThreadId,
    },
    ThreadCheckpointList {
        thread_id: ThreadId,
    },
    ThreadCheckpointEvents {
        checkpoint_id: CheckpointId,
    },
    ThreadFork {
        thread_id: ThreadId,
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
        display_name: Option<String>,
    },
    ThreadReplaceHistory {
        thread_id: ThreadId,
        target: HistoryTarget,
    },
    ThreadCancel {
        thread_id: ThreadId,
    },
    ThreadProcessList {
        thread_id: ThreadId,
    },
    ThreadProcessInspect {
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
    },
    CodexLogin,
    CodexLogout,
    CodexLoginStatus,
    CodexRateLimits,
    ApprovalAllow {
        approval_id: ApprovalId,
    },
    ApprovalDeny {
        approval_id: ApprovalId,
        reason: Option<String>,
    },
    RunnerList,
    RunnerLaunch {
        name: String,
        description: String,
        approval: ApprovalPolicy,
        command: Vec<String>,
    },
    ExecCommand {
        thread_id: ThreadId,
        runner: String,
        command: String,
        mode: CommandMode,
    },
    WaitProcess {
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
        timeout_ms: u64,
    },
    StopProcess {
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
    },
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControllerResponse {
    Running,
    Stopping,
    ThreadCreated {
        thread_id: ThreadId,
    },
    ThreadList {
        threads: Vec<Thread>,
    },
    ModelList {
        models: Vec<Model>,
    },
    ThreadRenamed,
    ThreadModelChanged,
    TurnStarted {
        thread_id: ThreadId,
    },
    TurnDelta {
        content: String,
    },
    ReasoningSummaryDelta {
        content: String,
    },
    ReasoningSummaryPartAdded,
    WebSearchUpdate {
        item_id: String,
        action: Option<Value>,
    },
    ToolCallStarted {
        item_id: String,
        name: String,
    },
    ToolCallDelta {
        item_id: String,
        delta: String,
    },
    TurnEvent {
        event: ThreadEvent,
    },
    TurnCompleted {
        content: String,
    },
    ThreadCancelled,
    ThreadNotActive,
    ThreadProcessList {
        processes: Vec<BackgroundProcess>,
    },
    ThreadProcessInspect {
        process: BackgroundProcessDetail,
    },
    ApprovalResolved,
    ApprovalRequired {
        approval_id: ApprovalId,
        thread_id: ThreadId,
        tool: String,
        arguments: Value,
        operation_index: Option<usize>,
        operation_label: Option<String>,
    },
    RunnerOperationUpdate {
        call_id: String,
        operation_index: usize,
        update: RunnerOperationUpdate,
    },
    ThreadEvents {
        events: Vec<ThreadEvent>,
    },
    ThreadCheckpointCreated {
        checkpoint_id: CheckpointId,
    },
    ThreadCheckpointList {
        checkpoints: Vec<ThreadCheckpoint>,
    },
    ThreadCheckpointEvents {
        events: Vec<ThreadEvent>,
    },
    ThreadForked {
        thread_id: ThreadId,
    },
    ThreadHistoryReplaced,
    RunnerList {
        runners: Vec<Runner>,
    },
    Launched,
    AlreadyRunning,
    ProcessStarted {
        process_id: ProcessId,
    },
    ProcessRunning {
        process_id: ProcessId,
        output: String,
    },
    ProcessFinished {
        output: String,
        exit_code: Option<i32>,
    },
    ProcessStopped {
        output: String,
    },
    Error {
        message: String,
    },
    CodexLoginRequired,
    CodexLoggedIn {
        email: Option<String>,
    },
    CodexLoggedOut,
    CodexRateLimits {
        snapshots: Value,
    },
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunnerOperationUpdate {
    CommandStarted,
    CommandOutput {
        content: String,
        omitted_bytes: usize,
    },
    Completed {
        artifact: ToolArtifact,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ToolArtifact {
    CommandExecution(CommandExecutionArtifact),
    PatchOperations(ApplyPatchResult),
    RunnerOperation(RunnerOperationArtifact),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
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
pub struct RunnerOperationArtifact {
    pub operation: usize,
    pub runner: String,
    pub label: String,
    pub result: Value,
    pub artifacts: Vec<ToolArtifact>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ThreadEvent {
    pub sequence: EventSequence,
    #[serde(flatten)]
    pub data: ThreadEventData,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
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
pub struct MessageEvent {
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct AssistantMessageEvent {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<AssistantMessagePhase>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantMessagePhase {
    Commentary,
    FinalAnswer,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ItemEvent {
    pub item: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ModelOutputEvent {
    pub request_sequence: EventSequence,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(untagged)]
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
#[serde(untagged)]
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
pub struct FrozenBoundaryEvent {
    pub through_sequence: EventSequence,
    pub masked_sequences: Vec<EventSequence>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CompactionEvent {
    pub items: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ModelRequestEvent {
    pub kind: ModelRequestKind,
    pub started_at_ms: u64,
    pub request: Value,
    pub context_window: Option<i64>,
    pub auto_compact_token_limit: Option<i64>,
    pub compacted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequestKind {
    Compaction,
    Response,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TokenUsageEvent {
    pub request_sequence: EventSequence,
    pub usage: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RateLimitsEvent {
    pub request_sequence: EventSequence,
    pub snapshots: Value,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Thread {
    pub id: ThreadId,
    pub display_name: Option<String>,
    pub model: String,
    pub reasoning_effort: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ThreadCheckpoint {
    pub id: CheckpointId,
    pub thread_id: ThreadId,
    pub created_at_ms: i64,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
pub struct Runner {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Model {
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
pub struct BackgroundProcess {
    pub runner: String,
    pub process_id: ProcessId,
    pub command: String,
    pub started_at_ms: i64,
    pub status: ProcessStatus,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BackgroundProcessDetail {
    pub process: BackgroundProcess,
    pub output_tail: String,
    pub omitted_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
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
    WaitProcess {
        process_handle: ProcessHandle,
        timeout_ms: u64,
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
    },
    ProcessRunning {
        process_handle: ProcessHandle,
        output: CommandOutput,
        patch_results: Vec<ApplyPatchResult>,
        spawned_processes: Vec<SpawnedProcess>,
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
