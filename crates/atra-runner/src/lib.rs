use anyhow::{Context, Result};
use atra_protocol::{RunnerRequest, RunnerResponse};
use tokio::io::{self, AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub async fn run_stdio() -> Result<()> {
    serve(BufReader::new(io::stdin()), io::stdout()).await
}

async fn serve(
    mut reader: impl AsyncBufRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
) -> Result<()> {
    let mut request = String::new();
    reader
        .read_line(&mut request)
        .await
        .context("failed to read runner request")?;
    let request: RunnerRequest =
        serde_json::from_str(&request).context("failed to decode runner request")?;

    match request {
        RunnerRequest::Initialize => {
            let mut response = serde_json::to_vec(&RunnerResponse::Ready)
                .context("failed to encode runner response")?;
            response.push(b'\n');
            writer
                .write_all(&response)
                .await
                .context("failed to write runner response")?;
            writer
                .flush()
                .await
                .context("failed to flush runner stdout")?;
        }
    }

    let mut request = String::new();
    if reader
        .read_line(&mut request)
        .await
        .context("failed to read runner request")?
        != 0
    {
        anyhow::bail!("runner received an unsupported request after initialization");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, BufReader};

    use super::*;

    #[tokio::test]
    async fn initialize_reports_ready() {
        let input = BufReader::new(&b"{\"method\":\"initialize\"}\n"[..]);
        let mut output = Vec::new();

        serve(input, &mut output).await.unwrap();

        assert_eq!(output, b"{\"status\":\"ready\"}\n");
    }

    #[tokio::test]
    async fn unsupported_message_is_rejected_without_output() {
        let input = BufReader::new(&b"{\"method\":\"execute\"}\n"[..]);
        let (mut output_reader, output_writer) = tokio::io::duplex(64);

        let error = serve(input, output_writer).await.unwrap_err();
        let mut output = Vec::new();
        output_reader.read_to_end(&mut output).await.unwrap();

        assert!(format!("{error:#}").contains("unknown variant"));
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn ready_runner_stays_alive_until_controller_disconnects() {
        let (controller, runner) = tokio::io::duplex(64);
        let (controller_reader, mut controller_writer) = tokio::io::split(controller);
        let (runner_reader, runner_writer) = tokio::io::split(runner);
        let mut task = tokio::spawn(serve(BufReader::new(runner_reader), runner_writer));

        controller_writer
            .write_all(b"{\"method\":\"initialize\"}\n")
            .await
            .unwrap();
        let mut response = String::new();
        BufReader::new(controller_reader)
            .read_line(&mut response)
            .await
            .unwrap();

        assert_eq!(response, "{\"status\":\"ready\"}\n");
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut task)
                .await
                .is_err()
        );

        drop(controller_writer);
        task.await.unwrap().unwrap();
    }
}
