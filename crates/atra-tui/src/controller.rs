use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use atra_protocol::{ControllerRequest, ControllerResponse};
use tokio::sync::mpsc;

use crate::app::TurnUpdate;

pub(super) async fn request(
    endpoint: &Path,
    request: ControllerRequest,
) -> Result<ControllerResponse> {
    tokio::time::timeout(
        Duration::from_secs(300),
        atra_client::request(endpoint, &request),
    )
    .await
    .context("controller request timed out")?
}

pub(super) async fn request_stream(
    endpoint: &Path,
    request: ControllerRequest,
    thread_id: i64,
    updates: &mpsc::UnboundedSender<TurnUpdate>,
) -> Result<ControllerResponse> {
    let mut connection = atra_client::Connection::open(endpoint, &request).await?;
    loop {
        let response = connection.receive().await?;
        match response {
            ControllerResponse::TurnDelta { content } => {
                updates.send(TurnUpdate::Delta { thread_id, content }).ok();
            }
            ControllerResponse::ReasoningSummaryDelta { content } => {
                updates
                    .send(TurnUpdate::ReasoningSummaryDelta { thread_id, content })
                    .ok();
            }
            ControllerResponse::ReasoningSummaryPartAdded => {
                updates
                    .send(TurnUpdate::ReasoningSummaryPartAdded { thread_id })
                    .ok();
            }
            ControllerResponse::ToolCallStarted { item_id, name } => {
                updates
                    .send(TurnUpdate::ToolCallStarted {
                        thread_id,
                        item_id,
                        name,
                    })
                    .ok();
            }
            ControllerResponse::ToolCallDelta { item_id, delta } => {
                updates
                    .send(TurnUpdate::ToolCallDelta {
                        thread_id,
                        item_id,
                        content: delta,
                    })
                    .ok();
            }
            ControllerResponse::TurnEvent { event } => {
                updates.send(TurnUpdate::Event { thread_id, event }).ok();
            }
            ControllerResponse::ApprovalRequired {
                approval_id,
                thread_id,
                tool,
                arguments,
                operation_index,
                operation_label,
            } => {
                let runner = arguments
                    .get("runner")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                updates
                    .send(TurnUpdate::ApprovalRequired {
                        approval_id,
                        thread_id,
                        runner,
                        label: operation_label.unwrap_or(tool),
                        operation_index,
                    })
                    .ok();
            }
            ControllerResponse::RunnerOperationUpdate {
                call_id,
                operation_index,
                update,
            } => {
                updates
                    .send(TurnUpdate::RunnerOperationUpdate {
                        thread_id,
                        call_id,
                        operation_index,
                        update,
                    })
                    .ok();
            }
            response => return Ok(response),
        }
    }
}
