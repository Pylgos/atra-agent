use std::{
    collections::HashMap,
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
use atra_platform::PlatformBundle;
use atra_protocol::{
    ApprovalPolicy, ControllerRequest, ControllerResponse, Runner as RunnerInfo, RunnerRequest,
    RunnerRequestEnvelope, RunnerResponse, RunnerResponseEnvelope, ThreadEvent, TimeoutAction,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_login::{
    AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthRouteConfig, CLIENT_ID, ServerOptions,
    run_login_server,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, mpsc, oneshot, watch},
};

mod model;
#[allow(dead_code)]
mod storage;

use model::{DEFAULT_MODEL, ModelResponse, Provider};
use storage::{EventKind, Store};

pub async fn codex_login(auth_home: &Path) -> Result<()> {
    fs::create_dir_all(auth_home)
        .with_context(|| format!("failed to create auth directory {}", auth_home.display()))?;
    fs::set_permissions(auth_home, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "failed to set permissions on auth directory {}",
            auth_home.display()
        )
    })?;
    let route = AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
        OutboundProxyPolicy::ReqwestDefault,
    ));
    let server = run_login_server(ServerOptions::new(
        auth_home.to_owned(),
        CLIENT_ID.to_owned(),
        None,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        route,
    ))
    .context("failed to start Codex login")?;
    eprintln!("Open this URL to sign in:\n{}", server.auth_url);
    server
        .block_until_done()
        .await
        .context("Codex login failed")
}

pub async fn run(endpoint: &Path, database: &Path, auth_home: &Path) -> Result<()> {
    let store = Store::open(database)
        .await
        .with_context(|| format!("failed to open controller database {}", database.display()))?;
    let prompt_cache_namespace = format!(
        "{:x}",
        Sha256::digest(database.as_os_str().as_encoded_bytes())
    );
    let provider = match env::var_os("ATRA_FAKE_MODEL_SCRIPT") {
        Some(path) => Provider::fake(Path::new(&path))?,
        None => Provider::codex(auth_home.to_owned()).await,
    };
    let platform_bundle_path = match env::var_os("ATRA_PLATFORM_BUNDLE") {
        Some(path) => Some(path.into()),
        None => current_platform_bundle()?,
    };
    let platform_bundle = platform_bundle_path
        .map(|path| PlatformBundle::load(&path))
        .transpose()?
        .map(Arc::new);

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
        platform_bundle,
        auth_home: auth_home.to_owned(),
        prompt_cache_namespace,
    });

    let (shutdown, mut shutdown_requested) = watch::channel(false);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = Arc::clone(&state);
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle_client(stream, &state, &shutdown).await {
                            tracing::warn!(error = %format!("{error:#}"), "client request failed");
                        }
                    });
                }
                Err(error) => tracing::warn!(%error, "failed to accept controller connection"),
            },
            changed = shutdown_requested.changed() => {
                if changed.is_ok() && *shutdown_requested.borrow() {
                    tracing::info!("controller stopping");
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_client(
    mut stream: UnixStream,
    state: &State,
    shutdown: &watch::Sender<bool>,
) -> Result<()> {
    let mut request = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut request)
        .await
        .context("failed to read controller request")?;
    let request: ControllerRequest =
        serde_json::from_str(&request).context("failed to decode controller request")?;
    if request == ControllerRequest::Shutdown {
        let response = write_response(&mut stream, &ControllerResponse::Stopping).await;
        let closed = stream
            .shutdown()
            .await
            .context("failed to close shutdown response stream");
        drop(stream);
        shutdown.send_replace(true);
        response?;
        closed?;
        return Ok(());
    }
    if let ControllerRequest::ThreadSend { thread_id, message } = request {
        let (deltas, mut pending_deltas) = mpsc::unbounded_channel();
        let response = {
            let response = state.run_turn(thread_id, message, Some(&deltas));
            tokio::pin!(response);
            loop {
                tokio::select! {
                    response = &mut response => break response,
                    Some(content) = pending_deltas.recv() => {
                        write_response(&mut stream, &ControllerResponse::TurnDelta { content }).await?;
                    }
                }
            }
        };
        drop(deltas);
        while let Ok(content) = pending_deltas.try_recv() {
            write_response(&mut stream, &ControllerResponse::TurnDelta { content }).await?;
        }
        let response = response.unwrap_or_else(|error| ControllerResponse::Error {
            message: format!("{error:#}"),
        });
        return write_response(&mut stream, &response).await;
    }
    let response = match state.handle(request).await {
        Ok(response) => response,
        Err(error) => ControllerResponse::Error {
            message: format!("{error:#}"),
        },
    };
    write_response(&mut stream, &response).await
}

async fn write_response(stream: &mut UnixStream, response: &ControllerResponse) -> Result<()> {
    let mut response =
        serde_json::to_vec(response).context("failed to encode controller response")?;
    response.push(b'\n');
    stream
        .write_all(&response)
        .await
        .context("failed to write controller response")
}

struct State {
    runners: Mutex<HashMap<String, Arc<Runner>>>,
    store: Store,
    provider: Mutex<Provider>,
    approvals: Mutex<HashMap<u64, PendingApproval>>,
    next_approval_id: AtomicU64,
    platform_bundle: Option<Arc<PlatformBundle>>,
    auth_home: PathBuf,
    prompt_cache_namespace: String,
}

impl State {
    async fn codex_login_status(&self) -> Result<ControllerResponse> {
        match self.provider.lock().await.login_status().await {
            Some(email) => Ok(ControllerResponse::CodexLoggedIn { email }),
            None => Ok(ControllerResponse::CodexLoginRequired),
        }
    }

    async fn handle(&self, request: ControllerRequest) -> Result<ControllerResponse> {
        match request {
            ControllerRequest::Status => Ok(ControllerResponse::Running),
            ControllerRequest::Shutdown => unreachable!("shutdown is handled before dispatch"),
            ControllerRequest::ThreadCreate { display_name } => {
                let thread_id = self
                    .store
                    .create_thread(display_name, DEFAULT_MODEL.to_owned(), "medium".to_owned())
                    .await
                    .context("failed to create thread")?;
                Ok(ControllerResponse::ThreadCreated { thread_id })
            }
            ControllerRequest::ThreadList => {
                let threads = self
                    .store
                    .threads()
                    .await
                    .context("failed to list threads")?;
                Ok(ControllerResponse::ThreadList { threads })
            }
            ControllerRequest::ModelList => Ok(ControllerResponse::ModelList {
                models: self.provider.lock().await.models().await?,
            }),
            ControllerRequest::ThreadRename {
                thread_id,
                display_name,
            } => {
                if display_name.trim().is_empty() {
                    bail!("thread display name must not be empty");
                }
                self.store
                    .rename_thread(thread_id, display_name)
                    .await
                    .context("failed to rename thread")?;
                Ok(ControllerResponse::ThreadRenamed)
            }
            ControllerRequest::ThreadSetModel {
                thread_id,
                model,
                reasoning_effort,
            } => {
                if model.trim().is_empty() {
                    bail!("thread model must not be empty");
                }
                if reasoning_effort.trim().is_empty() {
                    bail!("reasoning effort must not be empty");
                }
                let models = self.provider.lock().await.models().await?;
                let selected = models
                    .iter()
                    .find(|candidate| candidate.id == model)
                    .with_context(|| format!("unknown model {model}"))?;
                if !selected
                    .supported_reasoning_efforts
                    .iter()
                    .any(|candidate| candidate == &reasoning_effort)
                {
                    bail!("reasoning effort {reasoning_effort} is not supported by model {model}");
                }
                self.store
                    .set_thread_model(thread_id, model, reasoning_effort)
                    .await
                    .context("failed to change thread model")?;
                Ok(ControllerResponse::ThreadModelChanged)
            }
            ControllerRequest::ThreadSend { thread_id, message } => {
                self.run_turn(thread_id, message, None).await
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
            ControllerRequest::CodexLogin => {
                codex_login(&self.auth_home).await?;
                self.provider.lock().await.reload_auth().await;
                self.codex_login_status().await
            }
            ControllerRequest::CodexLoginStatus => self.codex_login_status().await,
            ControllerRequest::ApprovalAllow { approval_id } => {
                self.resolve_approval(approval_id, true, None).await
            }
            ControllerRequest::ApprovalDeny {
                approval_id,
                reason,
            } => self.resolve_approval(approval_id, false, reason).await,
            ControllerRequest::RunnerList => Ok(ControllerResponse::RunnerList {
                runners: self.list_runners().await?,
            }),
            ControllerRequest::RunnerLaunch {
                name,
                description,
                approval,
                command,
            } => {
                self.launch_runner(name, description, approval, command)
                    .await
            }
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
            ControllerRequest::ApplyPatch { runner, patch, cwd } => {
                tracing::debug!(runner, cwd = cwd.as_deref(), "applying patch");
                self.runner(&runner)
                    .await?
                    .request(RunnerRequest::ApplyPatch { patch, cwd })
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

    async fn run_turn(
        &self,
        thread_id: i64,
        message: String,
        deltas: Option<&mpsc::UnboundedSender<String>>,
    ) -> Result<ControllerResponse> {
        self.store
            .name_thread_if_unnamed(thread_id, message.clone())
            .await
            .context("failed to name thread")?;
        self.store
            .append(
                thread_id,
                EventKind::UserMessage,
                json!({ "content": message }),
            )
            .await
            .context("failed to save user message")?;
        self.continue_turn(thread_id, deltas).await
    }

    async fn continue_turn(
        &self,
        thread_id: i64,
        deltas: Option<&mpsc::UnboundedSender<String>>,
    ) -> Result<ControllerResponse> {
        loop {
            let mut events = self
                .store
                .events(thread_id)
                .await
                .context("failed to load model history")?;
            let (model, reasoning_effort) = self
                .store
                .thread_model(thread_id)
                .await
                .context("failed to load thread model")?;
            let prompt_cache_key = format!("{}-{thread_id}", self.prompt_cache_namespace);
            let mut provider = self.provider.lock().await;
            let auto_compact_token_limit = provider
                .models()
                .await?
                .into_iter()
                .find(|candidate| candidate.id == model)
                .and_then(|model| model.auto_compact_token_limit);
            let active_history_start = events
                .iter()
                .rposition(|event| event.kind == EventKind::Compaction)
                .map_or(0, |index| index + 1);
            let active_tokens = events[active_history_start..]
                .iter()
                .rev()
                .find(|event| event.kind == EventKind::TokenUsage)
                .and_then(|event| event.payload["total_tokens"].as_i64());
            if active_tokens
                .zip(auto_compact_token_limit)
                .is_some_and(|(tokens, limit)| tokens >= limit)
            {
                let items = provider
                    .compact(&model, &reasoning_effort, &events, &prompt_cache_key)
                    .await?;
                if !items.is_empty() {
                    self.store
                        .append(thread_id, EventKind::Compaction, json!({ "items": items }))
                        .await
                        .context("failed to save compacted model history")?;
                    events = self
                        .store
                        .events(thread_id)
                        .await
                        .context("failed to reload compacted model history")?;
                }
            }
            let completion = provider
                .complete(
                    &model,
                    &reasoning_effort,
                    &events,
                    deltas,
                    &prompt_cache_key,
                )
                .await?;
            drop(provider);
            for item in completion.reasoning {
                self.store
                    .append(thread_id, EventKind::Reasoning, json!({ "item": item }))
                    .await
                    .context("failed to save encrypted reasoning")?;
            }
            if let Some(usage) = completion.token_usage {
                self.store
                    .append(
                        thread_id,
                        EventKind::TokenUsage,
                        serde_json::to_value(usage).context("failed to encode token usage")?,
                    )
                    .await
                    .context("failed to save token usage")?;
            }

            match completion.response {
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
                ModelResponse::ToolCall {
                    name,
                    arguments,
                    call_id,
                } => {
                    self.store
                        .append(
                            thread_id,
                            EventKind::ToolCall,
                            json!({
                                "name": &name,
                                "arguments": &arguments,
                                "call_id": &call_id,
                            }),
                        )
                        .await
                        .context("failed to save tool call")?;
                    match name.as_str() {
                        "exec_command" => {
                            let arguments: ExecCommandArguments = serde_json::from_value(arguments)
                                .context("fake model returned invalid exec_command arguments")?;
                            if let Some(response) = self
                                .route_tool(
                                    thread_id,
                                    name,
                                    call_id,
                                    ToolArguments::ExecCommand(arguments),
                                )
                                .await?
                            {
                                return Ok(response);
                            }
                        }
                        "apply_patch" => {
                            let arguments: ApplyPatchArguments = serde_json::from_value(arguments)
                                .context("fake model returned invalid apply_patch arguments")?;
                            if let Some(response) = self
                                .route_tool(
                                    thread_id,
                                    name,
                                    call_id,
                                    ToolArguments::ApplyPatch(arguments),
                                )
                                .await?
                            {
                                return Ok(response);
                            }
                        }
                        "list_runners" => {
                            let result = serde_json::to_value(ControllerResponse::RunnerList {
                                runners: self.list_runners().await?,
                            })
                            .context("failed to encode runner list")?;
                            self.save_tool_result(thread_id, &name, call_id.as_deref(), result)
                                .await?;
                        }
                        _ => bail!("model requested unsupported tool {name}"),
                    }
                }
            }
        }
    }

    async fn route_tool(
        &self,
        thread_id: i64,
        name: String,
        call_id: Option<String>,
        arguments: ToolArguments,
    ) -> Result<Option<ControllerResponse>> {
        let runner = self.runner(arguments.runner()).await?;
        if runner.approval().await == ApprovalPolicy::Ask {
            let approval_id = self.next_approval_id.fetch_add(1, Ordering::Relaxed) + 1;
            let arguments_json =
                serde_json::to_value(&arguments).context("failed to encode approval arguments")?;
            self.store
                .append(
                    thread_id,
                    EventKind::ApprovalRequest,
                    json!({
                        "approval_id": approval_id,
                        "tool": &name,
                        "arguments": &arguments_json,
                    }),
                )
                .await
                .context("failed to save approval request")?;
            self.approvals.lock().await.insert(
                approval_id,
                PendingApproval {
                    thread_id,
                    name: name.clone(),
                    call_id,
                    arguments,
                },
            );
            return Ok(Some(ControllerResponse::ApprovalRequired {
                approval_id,
                thread_id,
                tool: name,
                arguments: arguments_json,
            }));
        }
        let result = self.execute(arguments).await?;
        self.save_tool_result(thread_id, &name, call_id.as_deref(), result)
            .await?;
        Ok(None)
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
        self.save_tool_result(
            pending.thread_id,
            &pending.name,
            pending.call_id.as_deref(),
            result,
        )
        .await?;
        self.continue_turn(pending.thread_id, None).await
    }

    async fn execute(&self, arguments: ToolArguments) -> Result<serde_json::Value> {
        let response = match arguments {
            ToolArguments::ExecCommand(arguments) => {
                self.runner(&arguments.runner)
                    .await?
                    .request(RunnerRequest::ExecCommand {
                        command: arguments.command,
                        cwd: arguments.cwd,
                        background: arguments.background,
                        timeout_ms: arguments.timeout_ms,
                        timeout_action: arguments.timeout_action,
                    })
                    .await?
            }
            ToolArguments::ApplyPatch(arguments) => {
                self.runner(&arguments.runner)
                    .await?
                    .request(RunnerRequest::ApplyPatch {
                        patch: arguments.patch,
                        cwd: arguments.cwd,
                    })
                    .await?
            }
        };
        serde_json::to_value(response).context("failed to encode tool result")
    }

    async fn save_tool_result(
        &self,
        thread_id: i64,
        name: &str,
        call_id: Option<&str>,
        result: serde_json::Value,
    ) -> Result<()> {
        self.store
            .append(
                thread_id,
                EventKind::ToolResult,
                json!({ "name": name, "call_id": call_id, "result": result }),
            )
            .await
            .context("failed to save tool result")?;
        Ok(())
    }

    async fn launch_runner(
        &self,
        name: String,
        description: String,
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
                *runner.config.lock().await = RunnerConfig {
                    description,
                    approval,
                    _command: command,
                };
                return Ok(ControllerResponse::AlreadyRunning);
            }
            runners.remove(&name);
        }

        let runner = Arc::new(
            Runner::start(
                &name,
                description,
                approval,
                command,
                self.platform_bundle.as_deref(),
            )
            .await?,
        );
        runners.insert(name, runner);
        Ok(ControllerResponse::Launched)
    }

    async fn list_runners(&self) -> Result<Vec<RunnerInfo>> {
        let mut runners = self.runners.lock().await;
        let mut stopped = Vec::new();
        let mut result = Vec::new();
        for (name, runner) in runners.iter() {
            if runner
                .child
                .lock()
                .await
                .try_wait()
                .with_context(|| format!("failed to inspect runner {name}"))?
                .is_some()
            {
                stopped.push(name.clone());
                continue;
            }
            result.push(RunnerInfo {
                name: name.clone(),
                description: runner.config.lock().await.description.clone(),
            });
        }
        for name in stopped {
            runners.remove(&name);
        }
        result.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(result)
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

#[derive(Deserialize, serde::Serialize)]
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

#[derive(Deserialize, serde::Serialize)]
struct ApplyPatchArguments {
    runner: String,
    patch: String,
    cwd: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum ToolArguments {
    ExecCommand(ExecCommandArguments),
    ApplyPatch(ApplyPatchArguments),
}

impl ToolArguments {
    fn runner(&self) -> &str {
        match self {
            Self::ExecCommand(arguments) => &arguments.runner,
            Self::ApplyPatch(arguments) => &arguments.runner,
        }
    }
}

struct PendingApproval {
    thread_id: i64,
    name: String,
    call_id: Option<String>,
    arguments: ToolArguments,
}

fn return_running() -> TimeoutAction {
    TimeoutAction::ReturnRunning
}

struct Runner {
    config: Mutex<RunnerConfig>,
    child: Mutex<Child>,
    client: RunnerClient,
}

struct RunnerConfig {
    description: String,
    approval: ApprovalPolicy,
    _command: Vec<String>,
}

impl Runner {
    async fn start(
        name: &str,
        description: String,
        approval: ApprovalPolicy,
        command: Vec<String>,
        platform_bundle: Option<&PlatformBundle>,
    ) -> Result<Self> {
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
        let requested_tools = platform_bundle
            .map(PlatformBundle::tool_names)
            .unwrap_or_default();
        match client
            .request_raw(RunnerRequest::Initialize {
                tools: requested_tools,
            })
            .await?
        {
            RunnerResponse::Ready => {}
            RunnerResponse::ToolsRequired { names } => {
                let bundle =
                    platform_bundle.context("runner requested tools without a platform bundle")?;
                for tool_name in names {
                    let tool = bundle
                        .tool(&tool_name)
                        .with_context(|| format!("runner requested unknown tool {tool_name}"))?;
                    match client
                        .request_raw(RunnerRequest::InstallTool {
                            name: tool_name.clone(),
                            digest: tool.digest().to_owned(),
                            blob: STANDARD.encode(tool.compressed()),
                        })
                        .await?
                    {
                        RunnerResponse::ToolInstalled => {}
                        response => bail!(
                            "runner {name} returned an invalid install response for \
                             {tool_name}: {response:?}"
                        ),
                    }
                }
                match client.request_raw(RunnerRequest::FinishInitialize).await? {
                    RunnerResponse::Ready => {}
                    response => {
                        bail!("runner {name} returned an invalid readiness response: {response:?}")
                    }
                }
            }
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
            config: Mutex::new(RunnerConfig {
                description,
                approval,
                _command: command,
            }),
            child: Mutex::new(child),
            client,
        })
    }

    async fn request(&self, request: RunnerRequest) -> Result<ControllerResponse> {
        map_runner_response(self.client.request_raw(request).await?)
    }

    async fn approval(&self) -> ApprovalPolicy {
        self.config.lock().await.approval
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

    async fn request_raw(&self, request: RunnerRequest) -> Result<RunnerResponse> {
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
        Ok(response)
    }
}

fn map_runner_response(response: RunnerResponse) -> Result<ControllerResponse> {
    match response {
        RunnerResponse::Ready => Ok(ControllerResponse::Running),
        RunnerResponse::ToolsRequired { .. } | RunnerResponse::ToolInstalled => {
            bail!("runner returned an initialization response after becoming ready")
        }
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
        RunnerResponse::PatchApplied { output } => Ok(ControllerResponse::PatchApplied { output }),
        RunnerResponse::Error { message } => bail!("{message}"),
    }
}

fn current_platform_bundle() -> Result<Option<PathBuf>> {
    let platform = match env::consts::ARCH {
        "x86_64" => "x86_64-linux-musl",
        "aarch64" => "aarch64-linux-musl",
        _ => return Ok(None),
    };
    let platform_directory = xdg::BaseDirectories::new()
        .get_data_home()
        .context("cannot determine the XDG data directory")?
        .join("atra/platforms")
        .join(platform);
    let current = platform_directory.join("current");
    let digest = match fs::read_to_string(&current) {
        Ok(digest) => digest,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to read current platform bundle {}",
                    current.display()
                )
            });
        }
    };
    let digest = digest.trim();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("current platform bundle contains an invalid digest");
    }
    Ok(Some(
        platform_directory
            .join("bundles")
            .join(format!("{digest}.zip")),
    ))
}

struct SocketGuard<'a>(&'a Path);

impl Drop for SocketGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
