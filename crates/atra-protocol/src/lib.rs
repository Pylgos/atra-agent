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
#[serde(rename_all = "snake_case")]
pub enum TimeoutAction {
    ReturnRunning,
    Terminate,
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
        background: bool,
        timeout_ms: Option<u64>,
        timeout_action: TimeoutAction,
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
        artifact: Value,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ThreadEvent {
    pub sequence: i64,
    pub kind: String,
    pub payload: Value,
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
        background: bool,
        timeout_ms: Option<u64>,
        timeout_action: TimeoutAction,
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
