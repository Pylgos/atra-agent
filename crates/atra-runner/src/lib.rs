use anyhow::{Context, Result};
use atra_protocol::{RunnerRequest, RunnerResponse};
use tokio::{
    io::{self, AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
};

pub async fn run_stdio() -> Result<()> {
    serve(BufReader::new(io::stdin()), io::stdout()).await
}

async fn serve(
    mut reader: impl AsyncBufRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
) -> Result<()> {
    let mut request = String::new();
    if reader
        .read_line(&mut request)
        .await
        .context("failed to read runner initialize request")?
        == 0
    {
        anyhow::bail!("controller disconnected before initializing runner");
    }
    let initialize: RunnerRequest =
        serde_json::from_str(&request).context("failed to decode runner initialize request")?;
    match initialize {
        RunnerRequest::Initialize => write_response(&mut writer, &RunnerResponse::Ready).await?,
        RunnerRequest::ExecCommand { .. } => {
            anyhow::bail!("runner received a command before initialization")
        }
    }

    loop {
        request.clear();
        if reader
            .read_line(&mut request)
            .await
            .context("failed to read runner request")?
            == 0
        {
            return Ok(());
        }
        let request: RunnerRequest =
            serde_json::from_str(&request).context("failed to decode runner request")?;
        let response = match request {
            RunnerRequest::Initialize => anyhow::bail!("runner was initialized more than once"),
            RunnerRequest::ExecCommand { command, cwd } => {
                let mut child = Command::new("bash");
                child.args(["-lc", &command]);
                if let Some(cwd) = cwd {
                    child.current_dir(cwd);
                }
                let output = child
                    .output()
                    .await
                    .context("failed to execute command with bash")?;
                RunnerResponse::CommandFinished {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    exit_code: output.status.code(),
                }
            }
        };
        write_response(&mut writer, &response).await?;
    }
}

async fn write_response(
    writer: &mut (impl AsyncWrite + Unpin),
    response: &RunnerResponse,
) -> Result<()> {
    let mut response = serde_json::to_vec(response).context("failed to encode runner response")?;
    response.push(b'\n');
    writer
        .write_all(&response)
        .await
        .context("failed to write runner response")?;
    writer
        .flush()
        .await
        .context("failed to flush runner stdout")
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
    async fn command_before_initialize_is_rejected_without_output() {
        let input = BufReader::new(&b"{\"method\":\"execute\"}\n"[..]);
        let (mut output_reader, output_writer) = tokio::io::duplex(64);

        let error = serve(input, output_writer).await.unwrap_err();
        let mut output = Vec::new();
        output_reader.read_to_end(&mut output).await.unwrap();

        assert!(format!("{error:#}").contains("unknown variant"));
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn executes_a_foreground_command_in_the_requested_directory() {
        let directory = tempfile::tempdir().unwrap();
        let input = format!(
            "{{\"method\":\"initialize\"}}\n{{\"method\":\"exec_command\",\"command\":\"printf out; printf err >&2; pwd\",\"cwd\":{}}}\n",
            serde_json::to_string(directory.path().to_str().unwrap()).unwrap()
        );
        let mut output = Vec::new();

        serve(BufReader::new(input.as_bytes()), &mut output)
            .await
            .unwrap();

        let responses: Vec<RunnerResponse> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(responses[0], RunnerResponse::Ready);
        assert_eq!(
            responses[1],
            RunnerResponse::CommandFinished {
                stdout: format!("out{}\n", directory.path().display()),
                stderr: "err".to_owned(),
                exit_code: Some(0),
            }
        );
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
