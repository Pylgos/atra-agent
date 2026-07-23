use std::{error::Error, fs, os::unix::fs::PermissionsExt, path::Path};

use atra_protocol::{ControllerRequest, ControllerResponse};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

type ControllerError = Box<dyn Error + Send + Sync>;

pub async fn run(endpoint: &Path) -> Result<(), ControllerError> {
    if endpoint.exists() {
        match UnixStream::connect(endpoint).await {
            Ok(_) => return Err("controller is already running".into()),
            Err(_) => fs::remove_file(endpoint)?,
        }
    }

    let listener = UnixListener::bind(endpoint)?;
    fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600))?;
    let _socket = SocketGuard(endpoint);

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream).await {
                        eprintln!("atra controller: {error}");
                    }
                });
            }
            Err(error) => {
                eprintln!("atra controller: {error}");
            }
        }
    }
}

async fn handle_client(mut stream: UnixStream) -> Result<(), ControllerError> {
    let mut request = String::new();
    BufReader::new(&mut stream).read_line(&mut request).await?;
    let request: ControllerRequest = serde_json::from_str(&request)?;
    match request {
        ControllerRequest::Status => {
            let mut response = serde_json::to_vec(&ControllerResponse::Running)?;
            response.push(b'\n');
            stream.write_all(&response).await?;
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
