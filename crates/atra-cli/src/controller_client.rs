use std::path::Path;

use anyhow::{Context, Result};
use atra_protocol::{ControllerRequest, ControllerResponse};

pub(crate) fn not_running(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
        )
    })
}

pub(crate) async fn request(
    endpoint: &Path,
    request: ControllerRequest,
) -> Result<ControllerResponse> {
    let mut connection = match atra_client::Connection::open(endpoint, &request).await {
        Ok(connection) => connection,
        Err(error) if not_running(&error) => {
            return Err(error).context("controller is not running");
        }
        Err(error) => return Err(error),
    };
    loop {
        match connection.receive().await? {
            ControllerResponse::TurnDelta { .. }
            | ControllerResponse::ToolCallStarted { .. }
            | ControllerResponse::ToolCallDelta { .. }
            | ControllerResponse::TurnEvent { .. } => {}
            response => return Ok(response),
        }
    }
}
