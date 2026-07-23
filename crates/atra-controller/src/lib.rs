use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use anyhow::{Context, Result, bail};
use atra_protocol::{ControllerRequest, ControllerResponse};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

#[allow(dead_code)]
mod storage;

pub async fn run(endpoint: &Path, database: &Path) -> Result<()> {
    let _store = storage::Store::open(database)
        .await
        .with_context(|| format!("failed to open controller database {}", database.display()))?;

    if endpoint.exists() {
        match UnixStream::connect(endpoint).await {
            Ok(_) => bail!("controller is already running at {}", endpoint.display()),
            Err(_) => fs::remove_file(endpoint)
                .with_context(|| format!("failed to remove stale socket {}", endpoint.display()))?,
        }
    }

    let listener = UnixListener::bind(endpoint)
        .with_context(|| format!("failed to bind controller socket {}", endpoint.display()))?;
    fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "failed to set permissions on controller socket {}",
            endpoint.display()
        )
    })?;
    let _socket = SocketGuard(endpoint);

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream).await {
                        eprintln!("atra controller: {error:#}");
                    }
                });
            }
            Err(error) => {
                eprintln!("atra controller: {error}");
            }
        }
    }
}

async fn handle_client(mut stream: UnixStream) -> Result<()> {
    let mut request = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut request)
        .await
        .context("failed to read controller request")?;
    let request: ControllerRequest =
        serde_json::from_str(&request).context("failed to decode controller request")?;
    match request {
        ControllerRequest::Status => {
            let mut response = serde_json::to_vec(&ControllerResponse::Running)
                .context("failed to encode controller response")?;
            response.push(b'\n');
            stream
                .write_all(&response)
                .await
                .context("failed to write controller response")?;
        }
    }
    Ok(())
}

struct SocketGuard<'a>(&'a Path);

impl Drop for SocketGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
