use serde::{Deserialize, Serialize};

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
    Error {
        message: String,
    },
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
    Error {
        message: String,
    },
}
