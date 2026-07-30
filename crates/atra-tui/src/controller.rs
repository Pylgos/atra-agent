use anyhow::Result;
use atra_client::{TurnEvent, TurnResult, TurnStream};
use tokio::sync::mpsc;

use crate::app::TurnUpdate;

pub(super) async fn forward_turn(
    mut stream: TurnStream,
    updates: &mpsc::UnboundedSender<TurnUpdate>,
) -> Result<TurnResult> {
    loop {
        let update = stream.receive().await?;
        let finished = match &update.event {
            TurnEvent::Finished(result) => Some(result.clone()),
            _ => None,
        };
        updates.send(TurnUpdate::Stream(update)).ok();
        if let Some(result) = finished {
            return Ok(result);
        }
    }
}
