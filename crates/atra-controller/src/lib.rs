use std::{
    collections::HashMap,
    env, fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
use atra_protocol::{
    ApprovalPolicy, ControllerRequest, ControllerResponse, RunnerRequest, RunnerRequestEnvelope,
    RunnerResponse, RunnerResponseEnvelope, ThreadEvent, TimeoutAction,
};
use serde::Deserialize;
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
};

mod model;
#[allow(dead_code)]
mod storage;

use model::{FakeProvider, ModelResponse};
use storage::{EventKind, Store};

pub async fn run(endpoint: &Path, database: &Path) -> Result<()> {
    let store = Store::open(database)
        .await
        .with_context(|| format!("failed to open controller database {}", database.display()))?;
    let provider = env::var_os("ATRA_FAKE_MODEL_SCRIPT")
        .map(|path| FakeProvider::load(Path::new(&path)))
        .transpose()?;

    if endpoint.exists() {
        match UnixStream::connect(endpoint).await {
            Ok(_) => bail!("controller is already running at {}", endpoint.display()),
            Err(_) => fs::remove_file(endpoint)
                .with_context(|| format!("failed to remove stale socket {}", endpoint.display()))?,
        }
    }

    let listener = UnixListener::bind(endpoint)
        .with_context(|| format!("failed to bind controller socket {}", endpoint.display()))?;
    tracing::info!(
        endpoint = %endpoint.display(),
        database = %database.display(),
        "controller started"
    );
    fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "failed to set permissions on controller socket {}",
            endpoint.display()
        )
    })?;
    let _socket = SocketGuard(endpoint);
    let state = Arc::new(State {
        runners: Mutex::new(HashMap::new()),
        store,
        provider: Mutex::new(provider),
        approvals: Mutex::new(HashMap::new()),
        next_approval_id: AtomicU64::new(0),
    });

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, &state).await {
                        tracing::warn!(error = %format!("{error:#}"), "client request failed");
                    }
                });
            }
            Err(error) => tracing::warn!(%error, "failed to accept controller connection"),
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
    let response = match state.handle(request).await {
        Ok(response) => response,
        Err(error) => ControllerResponse::Error {
            message: format!("{error:#}"),
        },
    };
    let mut response =
        serde_json::to_vec(&response).context("failed to encode controller response")?;
    response.push(b'\n');
    stream
        .write_all(&response)
        .await
        .context("failed to write controller response")
}

struct State {
    runners: Mutex<HashMap<String, Arc<Runner>>>,
    store: Store,
    provider: Mutex<Option<FakeProvider>>,
    approvals: Mutex<HashMap<u64, PendingApproval>>,
    next_approval_id: AtomicU64,
}

impl State {
    async fn handle(&self, request: ControllerRequest) -> Result<ControllerResponse> {
        match request {
            ControllerRequest::Status => Ok(ControllerResponse::Running),
            ControllerRequest::ThreadCreate => {
                let thread_id = self
                    .store
                    .create_thread()
                    .await
                    .context("failed to create thread")?;
                Ok(ControllerResponse::ThreadCreated { thread_id })
            }
            ControllerRequest::ThreadSend { thread_id, message } => {
                self.run_turn(thread_id, message).await
            }
            ControllerRequest::ThreadEvents { thread_id } => {
                let events = self
                    .store
                    .events(thread_id)
                    .await
                    .context("failed to load thread events")?
                    .into_iter()
                    .map(|event| ThreadEvent {
                        sequence: event.sequence,
                        kind: event.kind.as_str().to_owned(),
                        payload: event.payload,
                    })
                    .collect();
                Ok(ControllerResponse::ThreadEvents { events })
            }
            ControllerRequest::ApprovalAllow { approval_id } => {
                self.resolve_approval(approval_id, true, None).await
            }
            ControllerRequest::ApprovalDeny {
                approval_id,
                reason,
            } => self.resolve_approval(approval_id, false, reason).await,
            ControllerRequest::RunnerLaunch {
                name,
                approval,
                command,
            } => self.launch_runner(name, approval, command).await,
            ControllerRequest::ExecCommand {
                runner,
                command,
                cwd,
                background,
                timeout_ms,
                timeout_action,
            } => {
                tracing::debug!(
                    runner,
                    %command,
                    cwd = cwd.as_deref(),
                    background,
                    ?timeout_ms,
                    ?timeout_action,
                    "executing command"
                );
                self.runner(&runner)
                    .await?
                    .request(RunnerRequest::ExecCommand {
                        command,
                        cwd,
                        background,
                        timeout_ms,
                        timeout_action,
                    })
                    .await
            }
            ControllerRequest::WaitProcess {
                runner,
                process_handle,
                timeout_ms,
            } => {
                self.runner(&runner)
                    .await?
                    .request(RunnerRequest::WaitProcess {
                        process_handle,
                        timeout_ms,
                    })
                    .await
            }
            ControllerRequest::WriteProcess {
                runner,
                process_handle,
                input,
            } => {
                tracing::info!(
                    runner,
                    process_handle,
                    input_bytes = input.len(),
                    "writing process input"
                );
                tracing::trace!(
                    runner,
                    process_handle,
                    input = %String::from_utf8_lossy(&input),
                    "process input"
                );
                self.runner(&runner)
                    .await?
                    .request(RunnerRequest::WriteProcess {
                        process_handle,
                        input,
                    })
                    .await
            }
            ControllerRequest::StopProcess {
                runner,
                process_handle,
            } => {
                self.runner(&runner)
                    .await?
                    .request(RunnerRequest::StopProcess { process_handle })
                    .await
            }
        }
    }

    async fn run_turn(&self, thread_id: i64, message: String) -> Result<ControllerResponse> {
        self.store
            .append(
                thread_id,
                EventKind::UserMessage,
                json!({ "content": message }),
            )
            .await
            .context("failed to save user message")?;
        self.continue_turn(thread_id).await
    }

    async fn continue_turn(&self, thread_id: i64) -> Result<ControllerResponse> {
        loop {
            let events = self
                .store
                .events(thread_id)
                .await
                .context("failed to load model history")?;
            let response = self
                .provider
                .lock()
                .await
                .as_mut()
                .context("no model provider is configured")?
                .complete(&events)?;

            match response {
                ModelResponse::AssistantMessage { content } => {
                    self.store
                        .append(
                            thread_id,
                            EventKind::AssistantMessage,
                            json!({ "content": content }),
                        )
                        .await
                        .context("failed to save assistant message")?;
                    return Ok(ControllerResponse::TurnCompleted { content });
                }
                ModelResponse::ToolCall { name, arguments } => {
                    self.store
                        .append(
                            thread_id,
                            EventKind::ToolCall,
                            json!({ "name": &name, "arguments": &arguments }),
                        )
                        .await
                        .context("failed to save tool call")?;
                    match name.as_str() {
                        "exec_command" => {
                            let arguments: ExecCommandArguments = serde_json::from_value(arguments)
                                .context("fake model returned invalid exec_command arguments")?;
                            let runner = self.runner(&arguments.runner).await?;
                            if runner.approval().await == ApprovalPolicy::Ask {
                                let approval_id =
                                    self.next_approval_id.fetch_add(1, Ordering::Relaxed) + 1;
                                self.store
                                    .append(
                                        thread_id,
                                        EventKind::ApprovalRequest,
                                        json!({
                                            "approval_id": approval_id,
                                            "runner": &arguments.runner,
                                            "command": &arguments.command,
                                            "cwd": &arguments.cwd,
                                        }),
                                    )
                                    .await
                                    .context("failed to save approval request")?;
                                let response = ControllerResponse::ApprovalRequired {
                                    approval_id,
                                    thread_id,
                                    runner: arguments.runner.clone(),
                                    command: arguments.command.clone(),
                                    cwd: arguments.cwd.clone(),
                                };
                                self.approvals.lock().await.insert(
                                    approval_id,
                                    PendingApproval {
                                        thread_id,
                                        arguments,
                                    },
                                );
                                return Ok(response);
                            }
                            let result = self.execute(arguments).await?;
                            self.save_tool_result(thread_id, &name, result).await?;
                        }
                        _ => bail!("model requested unsupported tool {name}"),
                    }
                }
            }
        }
    }

    async fn resolve_approval(
        &self,
        approval_id: u64,
        allowed: bool,
        reason: Option<String>,
    ) -> Result<ControllerResponse> {
        let pending = self
            .approvals
            .lock()
            .await
            .remove(&approval_id)
            .with_context(|| format!("approval {approval_id} is not pending"))?;
        self.store
            .append(
                pending.thread_id,
                EventKind::ApprovalResponse,
                json!({
                    "approval_id": approval_id,
                    "decision": if allowed { "allow" } else { "deny" },
                    "reason": &reason,
                }),
            )
            .await
            .context("failed to save approval response")?;

        let result = if allowed {
            self.execute(pending.arguments).await?
        } else {
            let output = match reason {
                Some(reason) => format!("user denied the tool call: {reason}"),
                None => "user denied the tool call".to_owned(),
            };
            json!({ "status": "denied", "output": output })
        };
        self.save_tool_result(pending.thread_id, "exec_command", result)
            .await?;
        self.continue_turn(pending.thread_id).await
    }

    async fn execute(&self, arguments: ExecCommandArguments) -> Result<serde_json::Value> {
        let response = self
            .runner(&arguments.runner)
            .await?
            .request(RunnerRequest::ExecCommand {
                command: arguments.command,
                cwd: arguments.cwd,
                background: arguments.background,
                timeout_ms: arguments.timeout_ms,
                timeout_action: arguments.timeout_action,
            })
            .await?;
        serde_json::to_value(response).context("failed to encode tool result")
    }

    async fn save_tool_result(
        &self,
        thread_id: i64,
        name: &str,
        result: serde_json::Value,
    ) -> Result<()> {
        self.store
            .append(
                thread_id,
                EventKind::ToolResult,
                json!({ "name": name, "result": result }),
            )
            .await
            .context("failed to save tool result")?;
        Ok(())
    }

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
        if let Some(runner) = runners.get(&name) {
            if runner
                .child
                .lock()
                .await
                .try_wait()
                .with_context(|| format!("failed to inspect runner {name}"))?
                .is_none()
            {
                *runner.config.lock().await = (approval, command);
                return Ok(ControllerResponse::AlreadyRunning);
            }
            runners.remove(&name);
        }

        let runner = Arc::new(Runner::start(&name, approval, command).await?);
        runners.insert(name, runner);
        Ok(ControllerResponse::Launched)
    }

    async fn runner(&self, name: &str) -> Result<Arc<Runner>> {
        self.runners
            .lock()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("runner {name} is not running"))
    }
}

#[derive(Deserialize)]
struct ExecCommandArguments {
    runner: String,
    command: String,
    cwd: Option<String>,
    #[serde(default)]
    background: bool,
    timeout_ms: Option<u64>,
    #[serde(default = "return_running")]
    timeout_action: TimeoutAction,
}

struct PendingApproval {
    thread_id: i64,
    arguments: ExecCommandArguments,
}

fn return_running() -> TimeoutAction {
    TimeoutAction::ReturnRunning
}

struct Runner {
    config: Mutex<(ApprovalPolicy, Vec<String>)>,
    child: Mutex<Child>,
    client: RunnerClient,
}

impl Runner {
    async fn start(name: &str, approval: ApprovalPolicy, command: Vec<String>) -> Result<Self> {
        tracing::info!(runner = name, executable = command[0], "starting runner");
        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start runner {name} using {}", command[0]))?;
        let stdin = child
            .stdin
            .take()
            .context("runner stdin was not available")?;
        let stdout = child
            .stdout
            .take()
            .context("runner stdout was not available")?;
        let stderr = child
            .stderr
            .take()
            .context("runner stderr was not available")?;

        let runner_name = name.to_owned();
        tokio::spawn(async move {
            let mut stderr = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match stderr.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        tracing::info!(
                            runner = runner_name,
                            message = line.trim_end(),
                            "runner log"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            runner = runner_name,
                            %error,
                            "failed to read runner log"
                        );
                        break;
                    }
                }
            }
        });

        let client = RunnerClient::new(stdin, stdout, name);
        match client.request(RunnerRequest::Initialize).await? {
            ControllerResponse::Running => {}
            response => bail!("runner {name} returned an invalid readiness response: {response:?}"),
        }
        if child
            .try_wait()
            .with_context(|| format!("failed to inspect runner {name}"))?
            .is_some()
        {
            return Err(anyhow!("runner {name} exited during initialization"));
        }
        tracing::info!(runner = name, "runner ready");

        Ok(Self {
            config: Mutex::new((approval, command)),
            child: Mutex::new(child),
            client,
        })
    }

    async fn request(&self, request: RunnerRequest) -> Result<ControllerResponse> {
        self.client.request(request).await
    }

    async fn approval(&self) -> ApprovalPolicy {
        self.config.lock().await.0
    }
}

struct RunnerClient {
    stdin: Mutex<ChildStdin>,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<RunnerResponse>>>>,
    next_request_id: AtomicU64,
    name: String,
}

impl RunnerClient {
    fn new(stdin: ChildStdin, stdout: tokio::process::ChildStdout, name: &str) -> Self {
        let pending = Arc::new(StdMutex::new(
            HashMap::<u64, oneshot::Sender<RunnerResponse>>::new(),
        ));
        let reader_pending = Arc::clone(&pending);
        let runner_name = name.to_owned();
        tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match stdout.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => match serde_json::from_str::<RunnerResponseEnvelope>(&line) {
                        Ok(envelope) => {
                            if let Some(sender) =
                                reader_pending.lock().unwrap().remove(&envelope.request_id)
                            {
                                let _ = sender.send(envelope.response);
                            } else {
                                tracing::warn!(
                                    runner = runner_name,
                                    request_id = envelope.request_id,
                                    "runner returned an unknown request ID"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                runner = runner_name,
                                %error,
                                "runner returned an invalid response"
                            );
                            break;
                        }
                    },
                    Err(error) => {
                        tracing::warn!(
                            runner = runner_name,
                            %error,
                            "failed to read runner response"
                        );
                        break;
                    }
                }
            }
            for (_, sender) in reader_pending.lock().unwrap().drain() {
                let _ = sender.send(RunnerResponse::Error {
                    message: format!("runner {runner_name} disconnected"),
                });
            }
        });

        Self {
            stdin: Mutex::new(stdin),
            pending,
            next_request_id: AtomicU64::new(0),
            name: name.to_owned(),
        }
    }

    async fn request(&self, request: RunnerRequest) -> Result<ControllerResponse> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(request_id, sender);

        let mut request = serde_json::to_vec(&RunnerRequestEnvelope {
            request_id,
            request,
        })
        .context("failed to encode runner request")?;
        request.push(b'\n');
        if let Err(error) = self.stdin.lock().await.write_all(&request).await {
            self.pending.lock().unwrap().remove(&request_id);
            return Err(error)
                .with_context(|| format!("failed to send request to runner {}", self.name));
        }

        let response = receiver
            .await
            .with_context(|| format!("runner {} disconnected", self.name))?;
        map_runner_response(response)
    }
}

fn map_runner_response(response: RunnerResponse) -> Result<ControllerResponse> {
    match response {
        RunnerResponse::Ready => Ok(ControllerResponse::Running),
        RunnerResponse::ProcessStarted { process_handle } => {
            Ok(ControllerResponse::ProcessStarted { process_handle })
        }
        RunnerResponse::ProcessRunning {
            process_handle,
            output,
        } => Ok(ControllerResponse::ProcessRunning {
            process_handle,
            output,
        }),
        RunnerResponse::ProcessFinished { output, exit_code } => {
            tracing::info!(?exit_code, output_bytes = output.len(), "process finished");
            Ok(ControllerResponse::ProcessFinished { output, exit_code })
        }
        RunnerResponse::ProcessTimedOut { output } => {
            Ok(ControllerResponse::ProcessTimedOut { output })
        }
        RunnerResponse::InputWritten => Ok(ControllerResponse::InputWritten),
        RunnerResponse::ProcessStopped { output } => {
            Ok(ControllerResponse::ProcessStopped { output })
        }
        RunnerResponse::Error { message } => bail!("{message}"),
    }
}

struct SocketGuard<'a>(&'a Path);

impl Drop for SocketGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
