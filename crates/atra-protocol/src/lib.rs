use std::{collections::BTreeMap, path::PathBuf};

use atra_patch::ApplyPatchResult;
use atra_store::TreeManifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    Timed { timeout_ms: u64 },
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ControllerRequest {
    Status,
    Shutdown,
    ThreadCreate {
        display_name: Option<String>,
    },
    ThreadList,
    ModelList,
    ThreadRename {
        thread_id: i64,
        display_name: String,
    },
    ThreadSetModel {
        thread_id: i64,
        model: String,
        reasoning_effort: String,
    },
    ThreadSend {
        thread_id: i64,
        message: String,
    },
    ThreadEvents {
        thread_id: i64,
    },
    ThreadCheckpointCreate {
        thread_id: i64,
    },
    ThreadCheckpointList {
        thread_id: i64,
    },
    ThreadCheckpointEvents {
        checkpoint_id: i64,
    },
    ThreadCheckpointRestore {
        thread_id: i64,
        checkpoint_id: i64,
    },
    ThreadFork {
        thread_id: i64,
        checkpoint_id: Option<i64>,
        sequence: i64,
        display_name: Option<String>,
    },
    ThreadRewind {
        thread_id: i64,
        checkpoint_id: Option<i64>,
        sequence: i64,
    },
    ThreadContinue {
        thread_id: i64,
    },
    ThreadCancel {
        thread_id: i64,
    },
    ThreadProcessList {
        thread_id: i64,
    },
    ThreadProcessInspect {
        thread_id: i64,
        runner: String,
        process_id: String,
    },
    ThreadProcessStop {
        thread_id: i64,
        runner: String,
        process_id: String,
    },
    CodexLogin,
    CodexLogout,
    CodexLoginStatus,
    ApprovalAllow {
        approval_id: u64,
    },
    ApprovalDeny {
        approval_id: u64,
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
        runner: String,
        command: String,
        mode: CommandMode,
    },
    WaitProcess {
        runner: String,
        process_handle: String,
        timeout_ms: u64,
    },
    StopProcess {
        runner: String,
        process_handle: String,
    },
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControllerResponse {
    Running,
    Stopping,
    ThreadCreated {
        thread_id: i64,
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
        thread_id: i64,
    },
    TurnDelta {
        content: String,
    },
    ReasoningSummaryDelta {
        content: String,
    },
    ReasoningSummaryPartAdded,
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
    ThreadProcessStopped,
    ApprovalResolved,
    ApprovalRequired {
        approval_id: u64,
        thread_id: i64,
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
        checkpoint_id: i64,
    },
    ThreadCheckpointList {
        checkpoints: Vec<ThreadCheckpoint>,
    },
    ThreadCheckpointEvents {
        events: Vec<ThreadEvent>,
    },
    ThreadCheckpointRestored,
    ThreadForked {
        thread_id: i64,
    },
    ThreadRewound,
    RunnerList {
        runners: Vec<Runner>,
    },
    Launched,
    AlreadyRunning,
    ProcessStarted {
        process_handle: String,
    },
    ProcessRunning {
        process_handle: String,
        output: String,
    },
    ProcessFinished {
        output: String,
        exit_code: Option<i32>,
    },
    ProcessTimedOut {
        output: String,
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
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunnerOperationUpdate {
    CommandStarted,
    WaitStarted,
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
    TimedOut {
        output: String,
        runner: String,
        full_output_path: PathBuf,
    },
    Stopped {
        output: String,
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
    pub sequence: i64,
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
    AssistantMessage(MessageEvent),
    WebSearch(ItemEvent),
    ToolCall(ToolCallEvent),
    ToolResult(ToolResultEvent),
    FrozenBoundary(FrozenBoundaryEvent),
    Reasoning(ItemEvent),
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
            Self::Compaction(_) => "compaction",
            Self::ModelRequest(_) => "model_request",
            Self::TokenUsage(_) => "token_usage",
            Self::RateLimits(_) => "rate_limits",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct InstructionEvent {
    pub content: Option<String>,
    pub transition: InstructionTransition,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionTransition {
    Initial,
    Replacement,
    Removal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RunnersEvent {
    pub runners: Vec<Runner>,
    pub transition: InstructionTransition,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct MessageEvent {
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ItemEvent {
    pub item: Value,
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
    pub through_sequence: i64,
    pub masked_sequences: Vec<i64>,
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
    pub request_sequence: i64,
    pub usage: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RateLimitsEvent {
    pub request_sequence: i64,
    pub snapshots: Value,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Thread {
    pub id: i64,
    pub display_name: Option<String>,
    pub model: String,
    pub reasoning_effort: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ThreadCheckpoint {
    pub id: i64,
    pub thread_id: i64,
    pub created_at_ms: i64,
    pub reason: String,
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
    pub process_id: String,
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
    ExecCommand {
        command: String,
        mode: CommandMode,
        environment: CommandEnvironment,
    },
    StartCommand {
        command: String,
        environment: CommandEnvironment,
    },
    ApplyPatch {
        patch: String,
    },
    WaitProcess {
        process_handle: String,
        timeout_ms: u64,
    },
    StopProcess {
        process_handle: String,
    },
    InspectProcess {
        process_handle: String,
    },
    ProcessStatus {
        process_handle: String,
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
        process_handle: String,
    },
    ProcessRunning {
        process_handle: String,
        output: CommandOutput,
    },
    ProcessFinished {
        output: CommandOutput,
        exit_code: Option<i32>,
    },
    ProcessTimedOut {
        output: CommandOutput,
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
