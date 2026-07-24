use std::path::Path;

use anyhow::{Context, Result};
use atra_protocol::{ControllerRequest, ControllerResponse};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

pub struct Connection {
    responses: Lines<BufReader<OwnedReadHalf>>,
    _request: OwnedWriteHalf,
}

impl Connection {
    pub async fn open(endpoint: &Path, request: &ControllerRequest) -> Result<Self> {
        let stream = UnixStream::connect(endpoint).await.with_context(|| {
            format!("failed to connect to controller at {}", endpoint.display())
        })?;
        let (reader, mut writer) = stream.into_split();
        let mut encoded =
            serde_json::to_vec(request).context("failed to encode controller request")?;
        encoded.push(b'\n');
        writer
            .write_all(&encoded)
            .await
            .context("failed to write controller request")?;
        Ok(Self {
            responses: BufReader::new(reader).lines(),
            _request: writer,
        })
    }

    pub async fn receive(&mut self) -> Result<ControllerResponse> {
        let response = self
            .responses
            .next_line()
            .await
            .context("failed to read controller response")?
            .context("controller closed the response stream")?;
        serde_json::from_str(&response).context("failed to decode controller response")
    }
}

pub async fn request(endpoint: &Path, request: &ControllerRequest) -> Result<ControllerResponse> {
    Connection::open(endpoint, request).await?.receive().await
}
