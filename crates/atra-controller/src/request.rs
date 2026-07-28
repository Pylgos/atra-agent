use atra_protocol::{
    ApprovalId, ApprovalPolicy, CheckpointId, CommandMode, ControllerRequest, EventSequence,
    ProcessHandle, ProcessId, ThreadId,
};

pub(super) enum Request {
    Turn(TurnRequest),
    Unary(UnaryRequest),
    Shutdown,
}

pub(super) enum TurnRequest {
    Send {
        thread_id: ThreadId,
        message: String,
    },
    Continue {
        thread_id: ThreadId,
    },
}

pub(super) enum UnaryRequest {
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
    ThreadCheckpointRestore {
        thread_id: ThreadId,
        checkpoint_id: CheckpointId,
    },
    ThreadFork {
        thread_id: ThreadId,
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
        display_name: Option<String>,
    },
    ThreadRewind {
        thread_id: ThreadId,
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
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
    ThreadProcessStop {
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
    },
    CodexLogin,
    CodexLogout,
    CodexLoginStatus,
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
        runner: String,
        command: String,
        mode: CommandMode,
    },
    WaitProcess {
        runner: String,
        process_handle: ProcessHandle,
        timeout_ms: u64,
    },
    StopProcess {
        runner: String,
        process_handle: ProcessHandle,
    },
}

impl From<ControllerRequest> for Request {
    fn from(request: ControllerRequest) -> Self {
        match request {
            ControllerRequest::Status => Self::Unary(UnaryRequest::Status),
            ControllerRequest::Shutdown => Self::Shutdown,
            ControllerRequest::ThreadCreate { display_name } => {
                Self::Unary(UnaryRequest::ThreadCreate { display_name })
            }
            ControllerRequest::ThreadList => Self::Unary(UnaryRequest::ThreadList),
            ControllerRequest::ModelList => Self::Unary(UnaryRequest::ModelList),
            ControllerRequest::ThreadRename {
                thread_id,
                display_name,
            } => Self::Unary(UnaryRequest::ThreadRename {
                thread_id,
                display_name,
            }),
            ControllerRequest::ThreadSetModel {
                thread_id,
                model,
                reasoning_effort,
            } => Self::Unary(UnaryRequest::ThreadSetModel {
                thread_id,
                model,
                reasoning_effort,
            }),
            ControllerRequest::ThreadSend { thread_id, message } => {
                Self::Turn(TurnRequest::Send { thread_id, message })
            }
            ControllerRequest::ThreadEvents { thread_id } => {
                Self::Unary(UnaryRequest::ThreadEvents { thread_id })
            }
            ControllerRequest::ThreadCheckpointCreate { thread_id } => {
                Self::Unary(UnaryRequest::ThreadCheckpointCreate { thread_id })
            }
            ControllerRequest::ThreadCheckpointList { thread_id } => {
                Self::Unary(UnaryRequest::ThreadCheckpointList { thread_id })
            }
            ControllerRequest::ThreadCheckpointEvents { checkpoint_id } => {
                Self::Unary(UnaryRequest::ThreadCheckpointEvents { checkpoint_id })
            }
            ControllerRequest::ThreadCheckpointRestore {
                thread_id,
                checkpoint_id,
            } => Self::Unary(UnaryRequest::ThreadCheckpointRestore {
                thread_id,
                checkpoint_id,
            }),
            ControllerRequest::ThreadFork {
                thread_id,
                checkpoint_id,
                sequence,
                display_name,
            } => Self::Unary(UnaryRequest::ThreadFork {
                thread_id,
                checkpoint_id,
                sequence,
                display_name,
            }),
            ControllerRequest::ThreadRewind {
                thread_id,
                checkpoint_id,
                sequence,
            } => Self::Unary(UnaryRequest::ThreadRewind {
                thread_id,
                checkpoint_id,
                sequence,
            }),
            ControllerRequest::ThreadContinue { thread_id } => {
                Self::Turn(TurnRequest::Continue { thread_id })
            }
            ControllerRequest::ThreadCancel { thread_id } => {
                Self::Unary(UnaryRequest::ThreadCancel { thread_id })
            }
            ControllerRequest::ThreadProcessList { thread_id } => {
                Self::Unary(UnaryRequest::ThreadProcessList { thread_id })
            }
            ControllerRequest::ThreadProcessInspect {
                thread_id,
                runner,
                process_id,
            } => Self::Unary(UnaryRequest::ThreadProcessInspect {
                thread_id,
                runner,
                process_id,
            }),
            ControllerRequest::ThreadProcessStop {
                thread_id,
                runner,
                process_id,
            } => Self::Unary(UnaryRequest::ThreadProcessStop {
                thread_id,
                runner,
                process_id,
            }),
            ControllerRequest::CodexLogin => Self::Unary(UnaryRequest::CodexLogin),
            ControllerRequest::CodexLogout => Self::Unary(UnaryRequest::CodexLogout),
            ControllerRequest::CodexLoginStatus => Self::Unary(UnaryRequest::CodexLoginStatus),
            ControllerRequest::ApprovalAllow { approval_id } => {
                Self::Unary(UnaryRequest::ApprovalAllow { approval_id })
            }
            ControllerRequest::ApprovalDeny {
                approval_id,
                reason,
            } => Self::Unary(UnaryRequest::ApprovalDeny {
                approval_id,
                reason,
            }),
            ControllerRequest::RunnerList => Self::Unary(UnaryRequest::RunnerList),
            ControllerRequest::RunnerLaunch {
                name,
                description,
                approval,
                command,
            } => Self::Unary(UnaryRequest::RunnerLaunch {
                name,
                description,
                approval,
                command,
            }),
            ControllerRequest::ExecCommand {
                runner,
                command,
                mode,
            } => Self::Unary(UnaryRequest::ExecCommand {
                runner,
                command,
                mode,
            }),
            ControllerRequest::WaitProcess {
                runner,
                process_handle,
                timeout_ms,
            } => Self::Unary(UnaryRequest::WaitProcess {
                runner,
                process_handle,
                timeout_ms,
            }),
            ControllerRequest::StopProcess {
                runner,
                process_handle,
            } => Self::Unary(UnaryRequest::StopProcess {
                runner,
                process_handle,
            }),
        }
    }
}
