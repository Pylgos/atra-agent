use std::path::Path;

use atra_client::Client;

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

pub(crate) fn client(endpoint: &Path) -> Client {
    Client::new(endpoint)
}
