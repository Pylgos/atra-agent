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
    CodexLogin,
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
        cwd: Option<String>,
        background: bool,
        timeout_ms: Option<u64>,
        timeout_action: TimeoutAction,
    },
    ApplyPatch {
        runner: String,
        patch: String,
    },
    WaitProcess {
        runner: String,
        process_handle: u64,
        timeout_ms: u64,
    },
    WriteProcess {
        runner: String,
        process_handle: u64,
        input: Vec<u8>,
    },
    StopProcess {
        runner: String,
        process_handle: u64,
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
    ApprovalRequired {
        approval_id: u64,
        thread_id: i64,
        tool: String,
        arguments: Value,
    },
    ThreadEvents {
        events: Vec<ThreadEvent>,
    },
    RunnerList {
        runners: Vec<Runner>,
    },
    Launched,
    AlreadyRunning,
    ProcessStarted {
        process_handle: u64,
    },
    ProcessRunning {
        process_handle: u64,
        output: String,
    },
    ProcessFinished {
        output: String,
        exit_code: Option<i32>,
    },
    ProcessTimedOut {
        output: String,
    },
    InputWritten,
    ProcessStopped {
        output: String,
    },
    PatchApplied {
        output: String,
    },
    Error {
        message: String,
    },
    CodexLoginRequired,
    CodexLoggedIn {
        email: Option<String>,
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RunnerRequest {
    Initialize {
        tools: Vec<String>,
    },
    InstallTool {
        name: String,
        digest: String,
        blob: String,
    },
    FinishInitialize,
    ExecCommand {
        command: String,
        cwd: Option<String>,
        background: bool,
        timeout_ms: Option<u64>,
        timeout_action: TimeoutAction,
    },
    ApplyPatch {
        patch: String,
    },
    WaitProcess {
        process_handle: u64,
        timeout_ms: u64,
    },
    WriteProcess {
        process_handle: u64,
        input: Vec<u8>,
    },
    StopProcess {
        process_handle: u64,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunnerResponse {
    Ready,
    ToolsRequired {
        names: Vec<String>,
    },
    ToolInstalled,
    ProcessStarted {
        process_handle: u64,
    },
    ProcessRunning {
        process_handle: u64,
        output: String,
    },
    ProcessFinished {
        output: String,
        exit_code: Option<i32>,
    },
    ProcessTimedOut {
        output: String,
    },
    InputWritten,
    ProcessStopped {
        output: String,
    },
    PatchResult {
        success: bool,
        message: String,
    },
    Error {
        message: String,
    },
}
