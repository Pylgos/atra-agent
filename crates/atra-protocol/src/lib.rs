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
    ThreadCreate,
    ThreadSend {
        thread_id: i64,
        message: String,
    },
    ThreadEvents {
        thread_id: i64,
    },
    ApprovalAllow {
        approval_id: u64,
    },
    ApprovalDeny {
        approval_id: u64,
        reason: Option<String>,
    },
    RunnerLaunch {
        name: String,
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
        cwd: Option<String>,
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
    ThreadCreated {
        thread_id: i64,
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
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ThreadEvent {
    pub sequence: i64,
    pub kind: String,
    pub payload: Value,
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
    Initialize,
    ExecCommand {
        command: String,
        cwd: Option<String>,
        background: bool,
        timeout_ms: Option<u64>,
        timeout_action: TimeoutAction,
    },
    ApplyPatch {
        patch: String,
        cwd: Option<String>,
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
}
