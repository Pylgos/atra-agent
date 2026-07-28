use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use atra_protocol::{
    ApprovalId, ApprovalPolicy, BackgroundProcess, BackgroundProcessDetail, CheckpointId,
    CommandMode, ControllerRequest, ControllerResponse, EventSequence, Model, ProcessHandle,
    ProcessId, Runner, RunnerOperationUpdate, Thread, ThreadCheckpoint, ThreadEvent, ThreadId,
};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

struct Connection {
    responses: Lines<BufReader<OwnedReadHalf>>,
    _request: OwnedWriteHalf,
}

pub struct Client {
    endpoint: PathBuf,
}

pub struct TurnStream {
    connection: Connection,
    approval_context: Option<ApprovalContext>,
}

pub struct ApprovalContext {
    pub thread_id: ThreadId,
    pub operation_index: Option<usize>,
    pub operation_label: Option<String>,
}

#[derive(Debug)]
pub enum CodexLoginStatus {
    LoggedIn { email: Option<String> },
    LoginRequired,
}

#[derive(Debug)]
pub enum CancelResult {
    Cancelled,
    NotActive,
}

#[derive(Debug)]
pub enum LaunchResult {
    Launched,
    AlreadyRunning,
}

#[derive(Debug)]
pub enum TurnResult {
    ApprovalResolved,
    Cancelled,
    Completed {
        content: String,
    },
    ApprovalRequired {
        approval_id: ApprovalId,
        tool: String,
        arguments: Value,
    },
}

#[derive(Debug)]
pub enum TurnUpdate {
    Started {
        thread_id: ThreadId,
    },
    Delta {
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
    RunnerOperation {
        call_id: String,
        operation_index: usize,
        update: RunnerOperationUpdate,
    },
    Event {
        event: ThreadEvent,
    },
    ApprovalRequired {
        approval_id: ApprovalId,
        tool: String,
        arguments: Value,
    },
    Finished(TurnResult),
}

#[derive(Debug)]
pub enum ProcessResult {
    Started {
        process_handle: ProcessHandle,
    },
    Running {
        process_handle: ProcessHandle,
        output: String,
    },
    Finished {
        output: String,
        exit_code: Option<i32>,
    },
    Stopped {
        output: String,
    },
}

impl Connection {
    async fn open(endpoint: &Path, request: &ControllerRequest) -> Result<Self> {
        let stream = UnixStream::connect(endpoint).await.with_context(|| {
            format!("failed to connect to controller at {}", endpoint.display())
        })?;
        let (reader, mut writer) = stream.into_split();
        let mut encoded =
            serde_json::to_vec(request).context("failed to encode controller request")?;
        encoded.push(b'\n');
        writer
            .write_all(&encoded)
            .await
            .context("failed to write controller request")?;
        Ok(Self {
            responses: BufReader::new(reader).lines(),
            _request: writer,
        })
    }

    async fn receive(&mut self) -> Result<ControllerResponse> {
        let response = self
            .responses
            .next_line()
            .await
            .context("failed to read controller response")?
            .context("controller closed the response stream")?;
        serde_json::from_str(&response).context("failed to decode controller response")
    }
}

impl TurnStream {
    pub fn take_approval_context(&mut self) -> Option<ApprovalContext> {
        self.approval_context.take()
    }

    pub async fn receive(&mut self) -> Result<TurnUpdate> {
        match self.connection.receive().await? {
            ControllerResponse::TurnStarted { thread_id } => Ok(TurnUpdate::Started { thread_id }),
            ControllerResponse::TurnDelta { content } => Ok(TurnUpdate::Delta { content }),
            ControllerResponse::ReasoningSummaryDelta { content } => {
                Ok(TurnUpdate::ReasoningSummaryDelta { content })
            }
            ControllerResponse::ReasoningSummaryPartAdded => {
                Ok(TurnUpdate::ReasoningSummaryPartAdded)
            }
            ControllerResponse::ToolCallStarted { item_id, name } => {
                Ok(TurnUpdate::ToolCallStarted { item_id, name })
            }
            ControllerResponse::ToolCallDelta { item_id, delta } => {
                Ok(TurnUpdate::ToolCallDelta { item_id, delta })
            }
            ControllerResponse::RunnerOperationUpdate {
                call_id,
                operation_index,
                update,
            } => Ok(TurnUpdate::RunnerOperation {
                call_id,
                operation_index,
                update,
            }),
            ControllerResponse::TurnEvent { event } => Ok(TurnUpdate::Event { event }),
            ControllerResponse::ApprovalRequired {
                approval_id,
                thread_id,
                tool,
                arguments,
                operation_index,
                operation_label,
            } => {
                self.approval_context = Some(ApprovalContext {
                    thread_id,
                    operation_index,
                    operation_label,
                });
                Ok(TurnUpdate::ApprovalRequired {
                    approval_id,
                    tool,
                    arguments,
                })
            }
            ControllerResponse::ApprovalResolved => {
                Ok(TurnUpdate::Finished(TurnResult::ApprovalResolved))
            }
            ControllerResponse::ThreadCancelled => Ok(TurnUpdate::Finished(TurnResult::Cancelled)),
            ControllerResponse::TurnCompleted { content } => {
                Ok(TurnUpdate::Finished(TurnResult::Completed { content }))
            }
            ControllerResponse::Error { message } => bail!("{message}"),
            response => unexpected(response),
        }
    }
}

impl Client {
    pub fn new(endpoint: impl Into<PathBuf>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    pub async fn status(&self) -> Result<()> {
        match self.unary(ControllerRequest::Status).await? {
            ControllerResponse::Running => Ok(()),
            response => unexpected(response),
        }
    }

    pub async fn thread_send(&self, thread_id: ThreadId, message: String) -> Result<TurnStream> {
        self.turn_stream(ControllerRequest::ThreadSend { thread_id, message })
            .await
    }

    pub async fn thread_continue(&self, thread_id: ThreadId) -> Result<TurnStream> {
        self.turn_stream(ControllerRequest::ThreadContinue { thread_id })
            .await
    }

    pub async fn codex_logout(&self) -> Result<()> {
        expect_unit(
            self.unary(ControllerRequest::CodexLogout).await?,
            ControllerResponse::CodexLoggedOut,
        )
    }

    pub async fn codex_login(&self) -> Result<CodexLoginStatus> {
        match self.unary(ControllerRequest::CodexLogin).await? {
            ControllerResponse::CodexLoggedIn { email } => Ok(CodexLoginStatus::LoggedIn { email }),
            ControllerResponse::CodexLoginRequired => Ok(CodexLoginStatus::LoginRequired),
            response => unexpected(response),
        }
    }

    pub async fn model_list(&self) -> Result<Vec<Model>> {
        match self.unary(ControllerRequest::ModelList).await? {
            ControllerResponse::ModelList { models } => Ok(models),
            response => unexpected(response),
        }
    }

    pub async fn codex_login_status(&self) -> Result<CodexLoginStatus> {
        match self.unary(ControllerRequest::CodexLoginStatus).await? {
            ControllerResponse::CodexLoggedIn { email } => Ok(CodexLoginStatus::LoggedIn { email }),
            ControllerResponse::CodexLoginRequired => Ok(CodexLoginStatus::LoginRequired),
            response => unexpected(response),
        }
    }

    pub async fn thread_create(&self, display_name: Option<String>) -> Result<ThreadId> {
        match self
            .unary(ControllerRequest::ThreadCreate { display_name })
            .await?
        {
            ControllerResponse::ThreadCreated { thread_id } => Ok(thread_id),
            response => unexpected(response),
        }
    }

    pub async fn thread_list(&self) -> Result<Vec<Thread>> {
        match self.unary(ControllerRequest::ThreadList).await? {
            ControllerResponse::ThreadList { threads } => Ok(threads),
            response => unexpected(response),
        }
    }

    pub async fn thread_rename(&self, thread_id: ThreadId, display_name: String) -> Result<()> {
        expect_unit(
            self.unary(ControllerRequest::ThreadRename {
                thread_id,
                display_name,
            })
            .await?,
            ControllerResponse::ThreadRenamed,
        )
    }

    pub async fn thread_set_model(
        &self,
        thread_id: ThreadId,
        model: String,
        reasoning_effort: String,
    ) -> Result<()> {
        expect_unit(
            self.unary(ControllerRequest::ThreadSetModel {
                thread_id,
                model,
                reasoning_effort,
            })
            .await?,
            ControllerResponse::ThreadModelChanged,
        )
    }

    pub async fn thread_events(&self, thread_id: ThreadId) -> Result<Vec<ThreadEvent>> {
        match self
            .unary(ControllerRequest::ThreadEvents { thread_id })
            .await?
        {
            ControllerResponse::ThreadEvents { events } => Ok(events),
            response => unexpected(response),
        }
    }

    pub async fn thread_cancel(&self, thread_id: ThreadId) -> Result<CancelResult> {
        match self
            .unary(ControllerRequest::ThreadCancel { thread_id })
            .await?
        {
            ControllerResponse::ThreadCancelled => Ok(CancelResult::Cancelled),
            ControllerResponse::ThreadNotActive => Ok(CancelResult::NotActive),
            response => unexpected(response),
        }
    }

    pub async fn thread_process_list(&self, thread_id: ThreadId) -> Result<Vec<BackgroundProcess>> {
        match self
            .unary(ControllerRequest::ThreadProcessList { thread_id })
            .await?
        {
            ControllerResponse::ThreadProcessList { processes } => Ok(processes),
            response => unexpected(response),
        }
    }

    pub async fn thread_process_inspect(
        &self,
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
    ) -> Result<BackgroundProcessDetail> {
        match self
            .unary(ControllerRequest::ThreadProcessInspect {
                thread_id,
                runner,
                process_id,
            })
            .await?
        {
            ControllerResponse::ThreadProcessInspect { process } => Ok(process),
            response => unexpected(response),
        }
    }

    pub async fn thread_process_stop(
        &self,
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
    ) -> Result<()> {
        expect_unit(
            self.unary(ControllerRequest::ThreadProcessStop {
                thread_id,
                runner,
                process_id,
            })
            .await?,
            ControllerResponse::ThreadProcessStopped,
        )
    }

    pub async fn checkpoint_create(&self, thread_id: ThreadId) -> Result<CheckpointId> {
        match self
            .unary(ControllerRequest::ThreadCheckpointCreate { thread_id })
            .await?
        {
            ControllerResponse::ThreadCheckpointCreated { checkpoint_id } => Ok(checkpoint_id),
            response => unexpected(response),
        }
    }

    pub async fn checkpoint_list(&self, thread_id: ThreadId) -> Result<Vec<ThreadCheckpoint>> {
        match self
            .unary(ControllerRequest::ThreadCheckpointList { thread_id })
            .await?
        {
            ControllerResponse::ThreadCheckpointList { checkpoints } => Ok(checkpoints),
            response => unexpected(response),
        }
    }

    pub async fn checkpoint_events(&self, checkpoint_id: CheckpointId) -> Result<Vec<ThreadEvent>> {
        match self
            .unary(ControllerRequest::ThreadCheckpointEvents { checkpoint_id })
            .await?
        {
            ControllerResponse::ThreadCheckpointEvents { events } => Ok(events),
            response => unexpected(response),
        }
    }

    pub async fn checkpoint_restore(
        &self,
        thread_id: ThreadId,
        checkpoint_id: CheckpointId,
    ) -> Result<()> {
        expect_unit(
            self.unary(ControllerRequest::ThreadCheckpointRestore {
                thread_id,
                checkpoint_id,
            })
            .await?,
            ControllerResponse::ThreadCheckpointRestored,
        )
    }

    pub async fn thread_fork(
        &self,
        thread_id: ThreadId,
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
        display_name: Option<String>,
    ) -> Result<ThreadId> {
        match self
            .unary(ControllerRequest::ThreadFork {
                thread_id,
                checkpoint_id,
                sequence,
                display_name,
            })
            .await?
        {
            ControllerResponse::ThreadForked { thread_id } => Ok(thread_id),
            response => unexpected(response),
        }
    }

    pub async fn thread_rewind(
        &self,
        thread_id: ThreadId,
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
    ) -> Result<()> {
        expect_unit(
            self.unary(ControllerRequest::ThreadRewind {
                thread_id,
                checkpoint_id,
                sequence,
            })
            .await?,
            ControllerResponse::ThreadRewound,
        )
    }

    pub async fn approval_allow(&self, approval_id: ApprovalId) -> Result<TurnResult> {
        decode_turn(
            self.unary(ControllerRequest::ApprovalAllow { approval_id })
                .await?,
        )
    }

    pub async fn approval_deny(
        &self,
        approval_id: ApprovalId,
        reason: Option<String>,
    ) -> Result<TurnResult> {
        decode_turn(
            self.unary(ControllerRequest::ApprovalDeny {
                approval_id,
                reason,
            })
            .await?,
        )
    }

    pub async fn runner_list(&self) -> Result<Vec<Runner>> {
        match self.unary(ControllerRequest::RunnerList).await? {
            ControllerResponse::RunnerList { runners } => Ok(runners),
            response => unexpected(response),
        }
    }

    pub async fn runner_launch(
        &self,
        name: String,
        description: String,
        approval: ApprovalPolicy,
        command: Vec<String>,
    ) -> Result<LaunchResult> {
        match self
            .unary(ControllerRequest::RunnerLaunch {
                name,
                description,
                approval,
                command,
            })
            .await?
        {
            ControllerResponse::Launched => Ok(LaunchResult::Launched),
            ControllerResponse::AlreadyRunning => Ok(LaunchResult::AlreadyRunning),
            response => unexpected(response),
        }
    }

    pub async fn exec_command(
        &self,
        runner: String,
        command: String,
        mode: CommandMode,
    ) -> Result<ProcessResult> {
        decode_process(
            self.unary(ControllerRequest::ExecCommand {
                runner,
                command,
                mode,
            })
            .await?,
        )
    }

    pub async fn wait_process(
        &self,
        runner: String,
        process_handle: ProcessHandle,
        timeout_ms: u64,
    ) -> Result<ProcessResult> {
        decode_process(
            self.unary(ControllerRequest::WaitProcess {
                runner,
                process_handle,
                timeout_ms,
            })
            .await?,
        )
    }

    pub async fn stop_process(
        &self,
        runner: String,
        process_handle: ProcessHandle,
    ) -> Result<ProcessResult> {
        decode_process(
            self.unary(ControllerRequest::StopProcess {
                runner,
                process_handle,
            })
            .await?,
        )
    }

    async fn unary(&self, request: ControllerRequest) -> Result<ControllerResponse> {
        let mut connection = Connection::open(&self.endpoint, &request).await?;
        loop {
            match connection.receive().await? {
                ControllerResponse::TurnDelta { .. }
                | ControllerResponse::ReasoningSummaryDelta { .. }
                | ControllerResponse::ReasoningSummaryPartAdded
                | ControllerResponse::ToolCallStarted { .. }
                | ControllerResponse::ToolCallDelta { .. }
                | ControllerResponse::RunnerOperationUpdate { .. }
                | ControllerResponse::TurnEvent { .. } => {}
                ControllerResponse::Error { message } => bail!("{message}"),
                response => return Ok(response),
            }
        }
    }

    async fn turn_stream(&self, request: ControllerRequest) -> Result<TurnStream> {
        Ok(TurnStream {
            connection: Connection::open(&self.endpoint, &request).await?,
            approval_context: None,
        })
    }
}

fn expect_unit(response: ControllerResponse, expected: ControllerResponse) -> Result<()> {
    if response == expected {
        Ok(())
    } else {
        unexpected(response)
    }
}

fn decode_turn(response: ControllerResponse) -> Result<TurnResult> {
    match response {
        ControllerResponse::ApprovalResolved => Ok(TurnResult::ApprovalResolved),
        ControllerResponse::ThreadCancelled => Ok(TurnResult::Cancelled),
        ControllerResponse::TurnCompleted { content } => Ok(TurnResult::Completed { content }),
        ControllerResponse::ApprovalRequired {
            approval_id,
            tool,
            arguments,
            ..
        } => Ok(TurnResult::ApprovalRequired {
            approval_id,
            tool,
            arguments,
        }),
        response => unexpected(response),
    }
}

fn decode_process(response: ControllerResponse) -> Result<ProcessResult> {
    match response {
        ControllerResponse::ProcessStarted { process_handle } => {
            Ok(ProcessResult::Started { process_handle })
        }
        ControllerResponse::ProcessRunning {
            process_handle,
            output,
        } => Ok(ProcessResult::Running {
            process_handle,
            output,
        }),
        ControllerResponse::ProcessFinished { output, exit_code } => {
            Ok(ProcessResult::Finished { output, exit_code })
        }
        ControllerResponse::ProcessStopped { output } => Ok(ProcessResult::Stopped { output }),
        response => unexpected(response),
    }
}

fn unexpected<T>(response: ControllerResponse) -> Result<T> {
    bail!("controller returned an unexpected response: {response:?}")
}
