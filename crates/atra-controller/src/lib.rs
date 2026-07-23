use std::{
    collections::HashMap, fs, os::unix::fs::PermissionsExt, path::Path, process::Stdio, sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use atra_protocol::{
    ApprovalPolicy, ControllerRequest, ControllerResponse, RunnerRequest, RunnerResponse,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
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
    let state = Arc::new(State::default());

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, &state).await {
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

async fn handle_client(mut stream: UnixStream, state: &State) -> Result<()> {
    let mut request = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut request)
        .await
        .context("failed to read controller request")?;
    let request: ControllerRequest =
        serde_json::from_str(&request).context("failed to decode controller request")?;
    let response = match request {
        ControllerRequest::Status => ControllerResponse::Running,
        ControllerRequest::RunnerLaunch {
            name,
            approval,
            command,
        } => match state.launch_runner(name, approval, command).await {
            Ok(response) => response,
            Err(error) => ControllerResponse::Error {
                message: format!("{error:#}"),
            },
        },
        ControllerRequest::ExecCommand {
            runner,
            command,
            cwd,
        } => match state.exec_command(&runner, command, cwd).await {
            Ok(response) => response,
            Err(error) => ControllerResponse::Error {
                message: format!("{error:#}"),
            },
        },
    };
    let mut response =
        serde_json::to_vec(&response).context("failed to encode controller response")?;
    response.push(b'\n');
    stream
        .write_all(&response)
        .await
        .context("failed to write controller response")?;
    Ok(())
}

#[derive(Default)]
struct State {
    runners: Mutex<HashMap<String, Runner>>,
}

impl State {
    async fn launch_runner(
        &self,
        name: String,
        approval: ApprovalPolicy,
        command: Vec<String>,
    ) -> Result<ControllerResponse> {
        if name.is_empty() {
            bail!("runner name must not be empty");
        }
        if command.is_empty() {
            bail!("runner command must not be empty");
        }

        let mut runners = self.runners.lock().await;
        if let Some(runner) = runners.get_mut(&name) {
            if runner
                .child
                .try_wait()
                .with_context(|| format!("failed to inspect runner {name}"))?
                .is_none()
            {
                runner.approval = approval;
                runner.command = command;
                return Ok(ControllerResponse::AlreadyRunning);
            }
            runners.remove(&name);
        }

        let runner = Runner::start(&name, approval, command).await?;
        runners.insert(name, runner);
        Ok(ControllerResponse::Launched)
    }

    async fn exec_command(
        &self,
        name: &str,
        command: String,
        cwd: Option<String>,
    ) -> Result<ControllerResponse> {
        let mut runners = self.runners.lock().await;
        let runner = runners
            .get_mut(name)
            .with_context(|| format!("runner {name} is not running"))?;
        runner.exec_command(name, command, cwd).await
    }
}

struct Runner {
    approval: ApprovalPolicy,
    command: Vec<String>,
    child: Child,
    #[allow(dead_code)]
    stdin: ChildStdin,
    #[allow(dead_code)]
    stdout: BufReader<ChildStdout>,
}

impl Runner {
    async fn start(name: &str, approval: ApprovalPolicy, command: Vec<String>) -> Result<Self> {
        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start runner {name} using {}", command[0]))?;
        let mut stdin = child
            .stdin
            .take()
            .context("runner stdin was not available")?;
        let stdout = child
            .stdout
            .take()
            .context("runner stdout was not available")?;
        let mut stdout = BufReader::new(stdout);

        let mut initialize = serde_json::to_vec(&RunnerRequest::Initialize)
            .context("failed to encode runner initialize request")?;
        initialize.push(b'\n');
        stdin
            .write_all(&initialize)
            .await
            .with_context(|| format!("failed to initialize runner {name}"))?;

        let mut response = String::new();
        stdout
            .read_line(&mut response)
            .await
            .with_context(|| format!("failed to read readiness from runner {name}"))?;
        let response: RunnerResponse = serde_json::from_str(&response)
            .with_context(|| format!("runner {name} returned an invalid readiness response"))?;
        match response {
            RunnerResponse::Ready => {}
            RunnerResponse::CommandFinished { .. } => {
                bail!("runner {name} returned a command result during initialization")
            }
        }
        if child
            .try_wait()
            .with_context(|| format!("failed to inspect runner {name}"))?
            .is_some()
        {
            return Err(anyhow!("runner {name} exited during initialization"));
        }

        Ok(Self {
            approval,
            command,
            child,
            stdin,
            stdout,
        })
    }

    async fn exec_command(
        &mut self,
        name: &str,
        command: String,
        cwd: Option<String>,
    ) -> Result<ControllerResponse> {
        let mut request = serde_json::to_vec(&RunnerRequest::ExecCommand { command, cwd })
            .context("failed to encode runner command")?;
        request.push(b'\n');
        self.stdin
            .write_all(&request)
            .await
            .with_context(|| format!("failed to send command to runner {name}"))?;

        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .await
            .with_context(|| format!("failed to read command result from runner {name}"))?;
        let response: RunnerResponse = serde_json::from_str(&response)
            .with_context(|| format!("runner {name} returned an invalid command response"))?;
        match response {
            RunnerResponse::CommandFinished {
                stdout,
                stderr,
                exit_code,
            } => Ok(ControllerResponse::CommandFinished {
                stdout,
                stderr,
                exit_code,
            }),
            RunnerResponse::Ready => bail!("runner {name} returned an unexpected ready response"),
        }
    }
}

struct SocketGuard<'a>(&'a Path);

impl Drop for SocketGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
