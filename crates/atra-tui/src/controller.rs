use anyhow::Result;
use atra_client::{TurnResult, TurnStream};
use atra_protocol::ThreadId;
use tokio::sync::mpsc;

use crate::app::TurnUpdate;

pub(super) async fn forward_turn(
    mut stream: TurnStream,
    thread_id: ThreadId,
    updates: &mpsc::UnboundedSender<TurnUpdate>,
) -> Result<TurnResult> {
    loop {
        match stream.receive().await? {
            atra_client::TurnUpdate::Started { thread_id } => {
                updates.send(TurnUpdate::StreamStarted { thread_id }).ok();
            }
            atra_client::TurnUpdate::Delta { content } => {
                updates.send(TurnUpdate::Delta { thread_id, content }).ok();
            }
            atra_client::TurnUpdate::ReasoningSummaryDelta { content } => {
                updates
                    .send(TurnUpdate::ReasoningSummaryDelta { thread_id, content })
                    .ok();
            }
            atra_client::TurnUpdate::ReasoningSummaryPartAdded => {
                updates
                    .send(TurnUpdate::ReasoningSummaryPartAdded { thread_id })
                    .ok();
            }
            atra_client::TurnUpdate::ToolCallStarted { item_id, name } => {
                updates
                    .send(TurnUpdate::ToolCallStarted {
                        thread_id,
                        item_id,
                        name,
                    })
                    .ok();
            }
            atra_client::TurnUpdate::ToolCallDelta { item_id, delta } => {
                updates
                    .send(TurnUpdate::ToolCallDelta {
                        thread_id,
                        item_id,
                        content: delta,
                    })
                    .ok();
            }
            atra_client::TurnUpdate::RunnerOperation {
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
            atra_client::TurnUpdate::Event { event } => {
                updates.send(TurnUpdate::Event { thread_id, event }).ok();
            }
            atra_client::TurnUpdate::ApprovalRequired {
                approval_id,
                tool,
                arguments,
            } => {
                let context = stream
                    .take_approval_context()
                    .expect("approval update includes context");
                let runner = arguments
                    .get("runner")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                updates
                    .send(TurnUpdate::ApprovalRequired {
                        approval_id,
                        thread_id: context.thread_id,
                        runner,
                        label: context.operation_label.unwrap_or(tool),
                        operation_index: context.operation_index,
                    })
                    .ok();
            }
            atra_client::TurnUpdate::Finished(result) => return Ok(result),
        }
    }
}
