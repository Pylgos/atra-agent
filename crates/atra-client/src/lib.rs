use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use atra_protocol::{
    CheckpointId, CheckpointState, CheckpointSubscriptionMessage, Command, CommandResponse,
    CommandResult, ControllerChange, ControllerState, ControllerSubscriptionMessage, ProcessChange,
    ProcessLocator, ProcessState, ProcessSubscriptionMessage, StateRequest, Subscribe,
    SubscriptionTerminal, ThreadChange, ThreadId, ThreadState, ThreadSubscriptionMessage,
};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

struct Connection {
    messages: Lines<BufReader<OwnedReadHalf>>,
    _request: OwnedWriteHalf,
}

pub struct Client {
    endpoint: PathBuf,
}

pub struct ControllerSubscription {
    connection: Connection,
    state: ControllerState,
}

pub struct ThreadSubscription {
    connection: Connection,
    state: ThreadState,
}

pub struct CheckpointSubscription {
    connection: Connection,
    state: CheckpointState,
}

pub struct ProcessSubscription {
    connection: Connection,
    state: ProcessState,
}

#[derive(Debug)]
pub struct SubscriptionError {
    terminal: SubscriptionTerminal,
}

impl SubscriptionError {
    pub fn terminal(&self) -> &SubscriptionTerminal {
        &self.terminal
    }
}

impl std::fmt::Display for SubscriptionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.terminal {
            SubscriptionTerminal::Deleted => formatter.write_str("subscribed resource was deleted"),
            SubscriptionTerminal::ControllerShutdown => {
                formatter.write_str("controller is shutting down")
            }
            SubscriptionTerminal::Error { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SubscriptionError {}

impl Connection {
    async fn open(endpoint: &Path, request: &StateRequest) -> Result<Self> {
        let stream = UnixStream::connect(endpoint).await.with_context(|| {
            format!("failed to connect to controller at {}", endpoint.display())
        })?;
        let (reader, mut writer) = stream.into_split();
        write_json_line(&mut writer, request)
            .await
            .context("failed to write controller request")?;
        Ok(Self {
            messages: BufReader::new(reader).lines(),
            _request: writer,
        })
    }

    async fn receive<M: DeserializeOwned>(&mut self) -> Result<M> {
        let message = self
            .messages
            .next_line()
            .await
            .context("failed to read controller message")?
            .context("controller closed the message stream")?;
        serde_json::from_str(&message).context("failed to decode controller message")
    }
}

async fn write_json_line(writer: &mut OwnedWriteHalf, message: &impl Serialize) -> Result<()> {
    let mut encoded = serde_json::to_vec(message).context("failed to encode controller request")?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    Ok(())
}

fn terminal_error(terminal: SubscriptionTerminal) -> anyhow::Error {
    anyhow::Error::new(SubscriptionError { terminal })
}

impl Client {
    pub fn new(endpoint: impl Into<PathBuf>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    pub async fn command(&self, command: Command) -> Result<CommandResult> {
        let mut connection =
            Connection::open(&self.endpoint, &StateRequest::Command(command)).await?;
        match connection.receive().await? {
            CommandResponse::Success { result } => Ok(result),
            CommandResponse::Error { message } => bail!(message),
        }
    }

    pub async fn subscribe_controller(&self) -> Result<ControllerSubscription> {
        let mut connection = Connection::open(
            &self.endpoint,
            &StateRequest::Subscribe(Subscribe::Controller {}),
        )
        .await?;
        let state = match connection.receive().await? {
            ControllerSubscriptionMessage::Snapshot { state } => state,
            ControllerSubscriptionMessage::Terminal { terminal } => {
                return Err(terminal_error(terminal));
            }
            ControllerSubscriptionMessage::Operation { .. } => {
                bail!("controller sent an operation before the subscription snapshot")
            }
        };
        Ok(ControllerSubscription { connection, state })
    }

    pub async fn subscribe_thread(&self, thread_id: ThreadId) -> Result<ThreadSubscription> {
        let mut connection = Connection::open(
            &self.endpoint,
            &StateRequest::Subscribe(Subscribe::Thread { thread_id }),
        )
        .await?;
        let state = match connection.receive().await? {
            ThreadSubscriptionMessage::Snapshot { state } => state,
            ThreadSubscriptionMessage::Terminal { terminal } => {
                return Err(terminal_error(terminal));
            }
            ThreadSubscriptionMessage::Operation { .. } => {
                bail!("controller sent an operation before the subscription snapshot")
            }
        };
        Ok(ThreadSubscription { connection, state })
    }

    pub async fn subscribe_checkpoint(
        &self,
        checkpoint_id: CheckpointId,
    ) -> Result<CheckpointSubscription> {
        let mut connection = Connection::open(
            &self.endpoint,
            &StateRequest::Subscribe(Subscribe::Checkpoint { checkpoint_id }),
        )
        .await?;
        let state = match connection.receive().await? {
            CheckpointSubscriptionMessage::Snapshot { state } => state,
            CheckpointSubscriptionMessage::Terminal { terminal } => {
                return Err(terminal_error(terminal));
            }
        };
        Ok(CheckpointSubscription { connection, state })
    }

    pub async fn subscribe_process(&self, process: ProcessLocator) -> Result<ProcessSubscription> {
        let mut connection = Connection::open(
            &self.endpoint,
            &StateRequest::Subscribe(Subscribe::Process { process }),
        )
        .await?;
        let state = match connection.receive().await? {
            ProcessSubscriptionMessage::Snapshot { state } => state,
            ProcessSubscriptionMessage::Terminal { terminal } => {
                return Err(terminal_error(terminal));
            }
            ProcessSubscriptionMessage::Operation { .. } => {
                bail!("controller sent an operation before the subscription snapshot")
            }
        };
        Ok(ProcessSubscription { connection, state })
    }
}

impl ControllerSubscription {
    pub fn state(&self) -> &ControllerState {
        &self.state
    }

    pub async fn receive(&mut self) -> Result<ControllerChange> {
        match self.connection.receive().await? {
            ControllerSubscriptionMessage::Operation { operation } => operation
                .apply(&mut self.state)
                .context("controller operation could not be applied"),
            ControllerSubscriptionMessage::Terminal { terminal } => Err(terminal_error(terminal)),
            ControllerSubscriptionMessage::Snapshot { .. } => {
                bail!("controller sent a second subscription snapshot")
            }
        }
    }
}

impl ThreadSubscription {
    pub fn state(&self) -> &ThreadState {
        &self.state
    }

    pub async fn receive(&mut self) -> Result<ThreadChange> {
        match self.connection.receive().await? {
            ThreadSubscriptionMessage::Operation { operation } => operation
                .apply(&mut self.state)
                .context("thread operation could not be applied"),
            ThreadSubscriptionMessage::Terminal { terminal } => Err(terminal_error(terminal)),
            ThreadSubscriptionMessage::Snapshot { .. } => {
                bail!("controller sent a second subscription snapshot")
            }
        }
    }
}

impl CheckpointSubscription {
    pub fn state(&self) -> &CheckpointState {
        &self.state
    }

    pub async fn receive_terminal(&mut self) -> Result<()> {
        match self.connection.receive().await? {
            CheckpointSubscriptionMessage::Terminal { terminal } => Err(terminal_error(terminal)),
            CheckpointSubscriptionMessage::Snapshot { .. } => {
                bail!("controller sent a second subscription snapshot")
            }
        }
    }
}

impl ProcessSubscription {
    pub fn state(&self) -> &ProcessState {
        &self.state
    }

    pub async fn receive(&mut self) -> Result<ProcessChange> {
        match self.connection.receive().await? {
            ProcessSubscriptionMessage::Operation { operation } => operation
                .apply(&mut self.state)
                .context("process operation could not be applied"),
            ProcessSubscriptionMessage::Terminal { terminal } => Err(terminal_error(terminal)),
            ProcessSubscriptionMessage::Snapshot { .. } => {
                bail!("controller sent a second subscription snapshot")
            }
        }
    }
}
