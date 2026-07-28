use anyhow::Result;
use atra_client::{TurnEvent, TurnStream};
use tokio::sync::mpsc;

use crate::app::TurnUpdate;

pub(super) async fn forward_turn(
    mut stream: TurnStream,
    updates: &mpsc::UnboundedSender<TurnUpdate>,
) -> Result<()> {
    loop {
        let update = stream.receive().await?;
        let finished = matches!(update.event, TurnEvent::Finished(_));
        updates.send(TurnUpdate::Stream(update)).ok();
        if finished {
            return Ok(());
        }
    }
}
