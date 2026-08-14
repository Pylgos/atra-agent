use std::{io::ErrorKind, sync::Arc};

use anyhow::{Context, Result};
use atra_protocol::{
    CheckpointSubscriptionMessage, Command, CommandResponse, CommandResult,
    ControllerSubscriptionMessage, ProcessSubscriptionMessage, StateRequest, Subscribe,
    SubscriptionTerminal, ThreadSubscriptionMessage,
};
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, watch},
};

use crate::State;

pub(crate) async fn handle_client(
    mut stream: UnixStream,
    state: Arc<State>,
    shutdown: &watch::Sender<bool>,
) -> Result<()> {
    let mut request = String::new();
    let bytes = BufReader::new(&mut stream)
        .read_line(&mut request)
        .await
        .context("failed to read controller request")?;
    if bytes == 0 {
        return Ok(());
    }
    let request = serde_json::from_str::<StateRequest>(&request)
        .context("failed to decode controller request")?;
    handle_state_request(stream, state, shutdown, request).await
}

async fn handle_state_request(
    mut stream: UnixStream,
    state: Arc<State>,
    shutdown: &watch::Sender<bool>,
    request: StateRequest,
) -> Result<()> {
    match request {
        StateRequest::Command(Command::Shutdown) => {
            let response = match state.shutdown().await {
                Ok(()) => CommandResponse::Success {
                    result: CommandResult::Accepted,
                },
                Err(error) => CommandResponse::Error {
                    message: format!("{error:#}"),
                },
            };
            write_message(&mut stream, &response).await?;
            stream.shutdown().await?;
            drop(stream);
            if matches!(response, CommandResponse::Success { .. }) {
                shutdown.send_replace(true);
            }
            Ok(())
        }
        StateRequest::Command(command) => {
            let response = match state.handle_command(command).await {
                Ok(result) => CommandResponse::Success { result },
                Err(error) => CommandResponse::Error {
                    message: format!("{error:#}"),
                },
            };
            write_message(&mut stream, &response).await
        }
        StateRequest::Subscribe(Subscribe::Controller {}) => {
            let receiver = state.views.subscribe_controller().await;
            drop(state);
            serve_subscription(stream, receiver).await
        }
        StateRequest::Subscribe(Subscribe::Thread { thread_id }) => {
            if let Err(error) = state.materialize_thread(thread_id).await {
                return write_subscription_error::<ThreadSubscriptionMessage>(&mut stream, error)
                    .await;
            }
            let receiver = match state.views.subscribe_thread(thread_id).await {
                Ok(receiver) => receiver,
                Err(error) => {
                    return write_subscription_error::<ThreadSubscriptionMessage>(
                        &mut stream,
                        error,
                    )
                    .await;
                }
            };
            drop(state);
            serve_subscription(stream, receiver).await
        }
        StateRequest::Subscribe(Subscribe::Checkpoint { checkpoint_id }) => {
            if let Err(error) = state.materialize_checkpoint(checkpoint_id).await {
                return write_subscription_error::<CheckpointSubscriptionMessage>(
                    &mut stream,
                    error,
                )
                .await;
            }
            let receiver = match state.views.subscribe_checkpoint(checkpoint_id).await {
                Ok(receiver) => receiver,
                Err(error) => {
                    return write_subscription_error::<CheckpointSubscriptionMessage>(
                        &mut stream,
                        error,
                    )
                    .await;
                }
            };
            drop(state);
            serve_subscription(stream, receiver).await
        }
        StateRequest::Subscribe(Subscribe::Process { process }) => {
            if let Err(error) = state.materialize_process(&process).await {
                return write_subscription_error::<ProcessSubscriptionMessage>(&mut stream, error)
                    .await;
            }
            let receiver = match state.views.subscribe_process(&process).await {
                Ok(receiver) => receiver,
                Err(error) => {
                    return write_subscription_error::<ProcessSubscriptionMessage>(
                        &mut stream,
                        error,
                    )
                    .await;
                }
            };
            drop(state);
            serve_subscription(stream, receiver).await
        }
    }
}

trait SubscriptionMessage: Serialize {
    fn terminal(terminal: SubscriptionTerminal) -> Self;
}

macro_rules! subscription_message {
    ($message:ty) => {
        impl SubscriptionMessage for $message {
            fn terminal(terminal: SubscriptionTerminal) -> Self {
                Self::Terminal { terminal }
            }
        }
    };
}

subscription_message!(ControllerSubscriptionMessage);
subscription_message!(ThreadSubscriptionMessage);
subscription_message!(CheckpointSubscriptionMessage);
subscription_message!(ProcessSubscriptionMessage);

async fn write_subscription_error<M: SubscriptionMessage>(
    stream: &mut UnixStream,
    error: anyhow::Error,
) -> Result<()> {
    write_message(
        stream,
        &M::terminal(SubscriptionTerminal::Error {
            message: format!("{error:#}"),
        }),
    )
    .await
}

async fn serve_subscription<M: SubscriptionMessage>(
    mut stream: UnixStream,
    mut messages: mpsc::UnboundedReceiver<M>,
) -> Result<()> {
    loop {
        tokio::select! {
            message = messages.recv() => match message {
                Some(message) => write_message(&mut stream, &message).await?,
                None => return Ok(()),
            },
            readable = stream.readable() => {
                readable.context("failed to monitor subscription peer")?;
                let mut byte = [0_u8; 1];
                match stream.try_read(&mut byte) {
                    Ok(0) => return Ok(()),
                    Ok(_) => {
                        write_message(
                            &mut stream,
                            &M::terminal(SubscriptionTerminal::Error {
                                message: "client sent a message after starting a subscription"
                                    .to_owned(),
                            }),
                        )
                        .await?;
                        return Ok(());
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error).context("failed to read subscription peer"),
                }
            }
        }
    }
}

async fn write_message(stream: &mut UnixStream, message: &impl Serialize) -> Result<()> {
    let mut response =
        serde_json::to_vec(message).context("failed to encode controller message")?;
    response.push(b'\n');
    stream
        .write_all(&response)
        .await
        .context("failed to write controller message")
}
