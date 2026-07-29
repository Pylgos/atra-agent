use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use atra_protocol::{
    ApprovalId, ApprovalPolicy, BackgroundProcess, BackgroundProcessDetail, CheckpointId,
    CommandMode, ControllerRequest, ControllerResponse, EventSequence, HistoryTarget, Model,
    ProcessId, Runner, RunnerOperationUpdate, Thread, ThreadCheckpoint, ThreadEvent, ThreadId,
    TurnRequest, UnaryRequest,
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
    thread_id: ThreadId,
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
pub struct TurnUpdate {
    pub thread_id: ThreadId,
    pub event: TurnEvent,
}

#[derive(Debug)]
pub enum TurnEvent {
    Started,
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
        operation_index: Option<usize>,
        operation_label: Option<String>,
    },
    Finished(TurnResult),
}

#[derive(Debug)]
pub enum ProcessResult {
    Started {
        process_id: ProcessId,
    },
    Running {
        process_id: ProcessId,
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
    pub async fn receive(&mut self) -> Result<TurnUpdate> {
        let event = match self.connection.receive().await? {
            ControllerResponse::TurnStarted { thread_id } => {
                self.ensure_thread(thread_id)?;
                TurnEvent::Started
            }
            ControllerResponse::TurnDelta { content } => TurnEvent::Delta { content },
            ControllerResponse::ReasoningSummaryDelta { content } => {
                TurnEvent::ReasoningSummaryDelta { content }
            }
            ControllerResponse::ReasoningSummaryPartAdded => TurnEvent::ReasoningSummaryPartAdded,
            ControllerResponse::ToolCallStarted { item_id, name } => {
                TurnEvent::ToolCallStarted { item_id, name }
            }
            ControllerResponse::ToolCallDelta { item_id, delta } => {
                TurnEvent::ToolCallDelta { item_id, delta }
            }
            ControllerResponse::RunnerOperationUpdate {
                call_id,
                operation_index,
                update,
            } => TurnEvent::RunnerOperation {
                call_id,
                operation_index,
                update,
            },
            ControllerResponse::TurnEvent { event } => TurnEvent::Event { event },
            ControllerResponse::ApprovalRequired {
                approval_id,
                thread_id,
                tool,
                arguments,
                operation_index,
                operation_label,
            } => {
                self.ensure_thread(thread_id)?;
                TurnEvent::ApprovalRequired {
                    approval_id,
                    tool,
                    arguments,
                    operation_index,
                    operation_label,
                }
            }
            ControllerResponse::ApprovalResolved => {
                TurnEvent::Finished(TurnResult::ApprovalResolved)
            }
            ControllerResponse::ThreadCancelled => TurnEvent::Finished(TurnResult::Cancelled),
            ControllerResponse::TurnCompleted { content } => {
                TurnEvent::Finished(TurnResult::Completed { content })
            }
            ControllerResponse::Error { message } => bail!("{message}"),
            response => return unexpected(response),
        };
        Ok(TurnUpdate {
            thread_id: self.thread_id,
            event,
        })
    }

    fn ensure_thread(&self, thread_id: ThreadId) -> Result<()> {
        if thread_id != self.thread_id {
            bail!(
                "controller returned update for thread {thread_id} on thread {} stream",
                self.thread_id
            );
        }
        Ok(())
    }
}

impl Client {
    pub fn new(endpoint: impl Into<PathBuf>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    pub async fn status(&self) -> Result<()> {
        match self.unary(UnaryRequest::Status).await? {
            ControllerResponse::Running => Ok(()),
            response => unexpected(response),
        }
    }

    pub async fn thread_send(&self, thread_id: ThreadId, message: String) -> Result<TurnStream> {
        self.turn_stream(TurnRequest::ThreadSend { thread_id, message })
            .await
    }

    pub async fn thread_continue(&self, thread_id: ThreadId) -> Result<TurnStream> {
        self.turn_stream(TurnRequest::ThreadContinue { thread_id })
            .await
    }

    pub async fn codex_logout(&self) -> Result<()> {
        expect_unit(
            self.unary(UnaryRequest::CodexLogout).await?,
            ControllerResponse::CodexLoggedOut,
        )
    }

    pub async fn codex_login(&self) -> Result<CodexLoginStatus> {
        match self.unary(UnaryRequest::CodexLogin).await? {
            ControllerResponse::CodexLoggedIn { email } => Ok(CodexLoginStatus::LoggedIn { email }),
            ControllerResponse::CodexLoginRequired => Ok(CodexLoginStatus::LoginRequired),
            response => unexpected(response),
        }
    }

    pub async fn model_list(&self) -> Result<Vec<Model>> {
        match self.unary(UnaryRequest::ModelList).await? {
            ControllerResponse::ModelList { models } => Ok(models),
            response => unexpected(response),
        }
    }

    pub async fn codex_login_status(&self) -> Result<CodexLoginStatus> {
        match self.unary(UnaryRequest::CodexLoginStatus).await? {
            ControllerResponse::CodexLoggedIn { email } => Ok(CodexLoginStatus::LoggedIn { email }),
            ControllerResponse::CodexLoginRequired => Ok(CodexLoginStatus::LoginRequired),
            response => unexpected(response),
        }
    }

    pub async fn codex_rate_limits(&self) -> Result<serde_json::Value> {
        match self.unary(UnaryRequest::CodexRateLimits).await? {
            ControllerResponse::CodexRateLimits { snapshots } => Ok(snapshots),
            response => unexpected(response),
        }
    }

    pub async fn thread_create(&self, display_name: Option<String>) -> Result<ThreadId> {
        match self
            .unary(UnaryRequest::ThreadCreate { display_name })
            .await?
        {
            ControllerResponse::ThreadCreated { thread_id } => Ok(thread_id),
            response => unexpected(response),
        }
    }

    pub async fn thread_list(&self) -> Result<Vec<Thread>> {
        match self.unary(UnaryRequest::ThreadList).await? {
            ControllerResponse::ThreadList { threads } => Ok(threads),
            response => unexpected(response),
        }
    }

    pub async fn thread_rename(&self, thread_id: ThreadId, display_name: String) -> Result<()> {
        expect_unit(
            self.unary(UnaryRequest::ThreadRename {
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
            self.unary(UnaryRequest::ThreadSetModel {
                thread_id,
                model,
                reasoning_effort,
            })
            .await?,
            ControllerResponse::ThreadModelChanged,
        )
    }

    pub async fn thread_events(&self, thread_id: ThreadId) -> Result<Vec<ThreadEvent>> {
        match self.unary(UnaryRequest::ThreadEvents { thread_id }).await? {
            ControllerResponse::ThreadEvents { events } => Ok(events),
            response => unexpected(response),
        }
    }

    pub async fn thread_cancel(&self, thread_id: ThreadId) -> Result<CancelResult> {
        match self.unary(UnaryRequest::ThreadCancel { thread_id }).await? {
            ControllerResponse::ThreadCancelled => Ok(CancelResult::Cancelled),
            ControllerResponse::ThreadNotActive => Ok(CancelResult::NotActive),
            response => unexpected(response),
        }
    }

    pub async fn thread_process_list(&self, thread_id: ThreadId) -> Result<Vec<BackgroundProcess>> {
        match self
            .unary(UnaryRequest::ThreadProcessList { thread_id })
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
            .unary(UnaryRequest::ThreadProcessInspect {
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

    pub async fn checkpoint_create(&self, thread_id: ThreadId) -> Result<CheckpointId> {
        match self
            .unary(UnaryRequest::ThreadCheckpointCreate { thread_id })
            .await?
        {
            ControllerResponse::ThreadCheckpointCreated { checkpoint_id } => Ok(checkpoint_id),
            response => unexpected(response),
        }
    }

    pub async fn checkpoint_list(&self, thread_id: ThreadId) -> Result<Vec<ThreadCheckpoint>> {
        match self
            .unary(UnaryRequest::ThreadCheckpointList { thread_id })
            .await?
        {
            ControllerResponse::ThreadCheckpointList { checkpoints } => Ok(checkpoints),
            response => unexpected(response),
        }
    }

    pub async fn checkpoint_events(&self, checkpoint_id: CheckpointId) -> Result<Vec<ThreadEvent>> {
        match self
            .unary(UnaryRequest::ThreadCheckpointEvents { checkpoint_id })
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
            self.unary(UnaryRequest::ThreadReplaceHistory {
                thread_id,
                target: HistoryTarget::Checkpoint { checkpoint_id },
            })
            .await?,
            ControllerResponse::ThreadHistoryReplaced,
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
            .unary(UnaryRequest::ThreadFork {
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
            self.unary(UnaryRequest::ThreadReplaceHistory {
                thread_id,
                target: HistoryTarget::Message {
                    checkpoint_id,
                    sequence,
                },
            })
            .await?,
            ControllerResponse::ThreadHistoryReplaced,
        )
    }

    pub async fn approval_allow(&self, approval_id: ApprovalId) -> Result<TurnResult> {
        decode_turn(
            self.unary(UnaryRequest::ApprovalAllow { approval_id })
                .await?,
        )
    }

    pub async fn approval_deny(
        &self,
        approval_id: ApprovalId,
        reason: Option<String>,
    ) -> Result<TurnResult> {
        decode_turn(
            self.unary(UnaryRequest::ApprovalDeny {
                approval_id,
                reason,
            })
            .await?,
        )
    }

    pub async fn runner_list(&self) -> Result<Vec<Runner>> {
        match self.unary(UnaryRequest::RunnerList).await? {
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
            .unary(UnaryRequest::RunnerLaunch {
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
        thread_id: ThreadId,
        runner: String,
        command: String,
        mode: CommandMode,
    ) -> Result<ProcessResult> {
        decode_process(
            self.unary(UnaryRequest::ExecCommand {
                thread_id,
                runner,
                command,
                mode,
            })
            .await?,
        )
    }

    pub async fn wait_process(
        &self,
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
        timeout_ms: u64,
    ) -> Result<ProcessResult> {
        decode_process(
            self.unary(UnaryRequest::WaitProcess {
                thread_id,
                runner,
                process_id,
                timeout_ms,
            })
            .await?,
        )
    }

    pub async fn stop_process(
        &self,
        thread_id: ThreadId,
        runner: String,
        process_id: ProcessId,
    ) -> Result<ProcessResult> {
        decode_process(
            self.unary(UnaryRequest::StopProcess {
                thread_id,
                runner,
                process_id,
            })
            .await?,
        )
    }

    async fn unary(&self, request: UnaryRequest) -> Result<ControllerResponse> {
        let request = ControllerRequest::Unary(request);
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

    async fn turn_stream(&self, request: TurnRequest) -> Result<TurnStream> {
        let thread_id = match &request {
            TurnRequest::ThreadSend { thread_id, .. }
            | TurnRequest::ThreadContinue { thread_id } => *thread_id,
        };
        let request = ControllerRequest::Turn(request);
        Ok(TurnStream {
            connection: Connection::open(&self.endpoint, &request).await?,
            thread_id,
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
        ControllerResponse::ProcessStarted { process_id } => {
            Ok(ProcessResult::Started { process_id })
        }
        ControllerResponse::ProcessRunning { process_id, output } => {
            Ok(ProcessResult::Running { process_id, output })
        }
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
