use anyhow::{Context, Result};
use atra_protocol::{ControllerRequest, ControllerResponse};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, watch},
};

use crate::{State, model::ModelStreamEvent};

pub(crate) async fn handle_client(
    mut stream: UnixStream,
    state: &State,
    shutdown: &watch::Sender<bool>,
) -> Result<()> {
    let mut request = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut request)
        .await
        .context("failed to read controller request")?;
    let request: ControllerRequest =
        serde_json::from_str(&request).context("failed to decode controller request")?;
    if request == ControllerRequest::Shutdown {
        let response = write_response(&mut stream, &ControllerResponse::Stopping).await;
        let closed = stream
            .shutdown()
            .await
            .context("failed to close shutdown response stream");
        drop(stream);
        shutdown.send_replace(true);
        response?;
        closed?;
        return Ok(());
    }
    if matches!(
        request,
        ControllerRequest::ThreadSend { .. }
            | ControllerRequest::ThreadContinue { .. }
            | ControllerRequest::ApprovalAllow { .. }
            | ControllerRequest::ApprovalDeny { .. }
    ) {
        let (updates, mut pending_updates) = mpsc::unbounded_channel();
        let response = {
            let response = state.handle_streaming(request, &updates);
            tokio::pin!(response);
            loop {
                tokio::select! {
                    response = &mut response => break response,
                    Some(update) = pending_updates.recv() => {
                        write_stream_update(&mut stream, update).await?;
                    }
                }
            }
        };
        drop(updates);
        while let Ok(update) = pending_updates.try_recv() {
            write_stream_update(&mut stream, update).await?;
        }
        let response = response.unwrap_or_else(|error| ControllerResponse::Error {
            message: format!("{error:#}"),
        });
        return write_response(&mut stream, &response).await;
    }
    let response = match state.handle(request).await {
        Ok(response) => response,
        Err(error) => ControllerResponse::Error {
            message: format!("{error:#}"),
        },
    };
    write_response(&mut stream, &response).await
}

async fn write_stream_update(stream: &mut UnixStream, update: ModelStreamEvent) -> Result<()> {
    let response = match update {
        ModelStreamEvent::AssistantDelta(content) => ControllerResponse::TurnDelta { content },
        ModelStreamEvent::ReasoningSummaryDelta(content) => {
            ControllerResponse::ReasoningSummaryDelta { content }
        }
        ModelStreamEvent::ReasoningSummaryPartAdded => {
            ControllerResponse::ReasoningSummaryPartAdded
        }
        ModelStreamEvent::ToolCallStarted { item_id, name } => {
            ControllerResponse::ToolCallStarted { item_id, name }
        }
        ModelStreamEvent::ToolCallDelta { item_id, delta } => {
            ControllerResponse::ToolCallDelta { item_id, delta }
        }
        ModelStreamEvent::ThreadEvent(event) => ControllerResponse::TurnEvent { event },
    };
    write_response(stream, &response).await
}

async fn write_response(stream: &mut UnixStream, response: &ControllerResponse) -> Result<()> {
    let mut response =
        serde_json::to_vec(response).context("failed to encode controller response")?;
    response.push(b'\n');
    stream
        .write_all(&response)
        .await
        .context("failed to write controller response")
}
