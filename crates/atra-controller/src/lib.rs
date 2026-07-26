use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use atra_patch::{ApplyPatchResult, PatchOperationOutcome, PatchOperationResult};
use atra_platform::PlatformStore;
use atra_protocol::{
    ApprovalPolicy, CommandEnvironment, CommandOutput, ControllerRequest, ControllerResponse,
    Runner as RunnerInfo, RunnerRequest, RunnerRequestEnvelope, RunnerResponse,
    RunnerResponseEnvelope, ThreadEvent, TimeoutAction,
};
use atra_store::{Store as AtraStore, TreeManifest};
use base64::{Engine, engine::general_purpose::STANDARD};
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_login::{
    AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthRouteConfig, CLIENT_ID, ServerOptions,
    default_client::set_default_originator, logout_with_revoke, run_login_server,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, mpsc, oneshot, watch},
    time::Instant,
};

mod connection;
mod model;
mod skills;
#[allow(dead_code)]
mod storage;

use model::{DEFAULT_MODEL, ModelResponse, ModelStreamEvent, Provider};
use storage::{EventKind, Store};

const WORKSPACE_INSTRUCTIONS_MAX_BYTES: usize = 32 * 1024;

pub async fn codex_login(auth_home: &Path) -> Result<()> {
    let _ = set_default_originator("atra".to_owned());
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

pub async fn codex_logout(auth_home: &Path) -> Result<()> {
    let _ = set_default_originator("atra".to_owned());
    let route = AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
        OutboundProxyPolicy::ReqwestDefault,
    ));
    logout_with_revoke(
        auth_home,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        &route,
    )
    .await
    .context("failed to log out of Codex")?;
    Ok(())
}

pub async fn run(endpoint: &Path, database: &Path, auth_home: &Path) -> Result<()> {
    let _ = set_default_originator("atra".to_owned());
    let workspace = env::current_dir().context("failed to determine controller workspace")?;
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
    let platform = current_platform()?.map(Arc::new);
    let data_home = xdg::BaseDirectories::new()
        .get_data_home()
        .context("cannot determine the XDG data directory")?;
    let skill_store =
        AtraStore::open(data_home.join("atra")).context("failed to open skill object store")?;

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
        provider,
        approvals: Mutex::new(HashMap::new()),
        active_turns: Mutex::new(HashMap::new()),
        thread_locks: StdMutex::new(HashMap::new()),
        next_approval_id: AtomicU64::new(0),
        platform,
        skill_store,
        skill_generation: Mutex::new(None),
        data_home,
        auth_home: auth_home.to_owned(),
        prompt_cache_namespace,
        workspace,
    });

    let (shutdown, mut shutdown_requested) = watch::channel(false);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = Arc::clone(&state);
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        if let Err(error) = connection::handle_client(stream, &state, &shutdown).await {
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

pub(crate) struct State {
    runners: Mutex<HashMap<String, Arc<Runner>>>,
    store: Store,
    provider: Provider,
    approvals: Mutex<HashMap<u64, PendingApproval>>,
    active_turns: Mutex<HashMap<i64, Arc<ActiveTurn>>>,
    thread_locks: StdMutex<HashMap<i64, Arc<Mutex<()>>>>,
    next_approval_id: AtomicU64,
    platform: Option<Arc<PlatformStore>>,
    skill_store: AtraStore,
    skill_generation: Mutex<Option<Arc<skills::SkillGeneration>>>,
    data_home: PathBuf,
    auth_home: PathBuf,
    prompt_cache_namespace: String,
    workspace: PathBuf,
}

#[derive(Clone)]
struct WorkspaceInstructions {
    content: Option<String>,
    tracked: bool,
}

struct ActiveTurn {
    cancel_requested: watch::Sender<bool>,
    cancellation: watch::Sender<Option<Result<(), String>>>,
    cancelling: AtomicBool,
    uncancellable: Mutex<()>,
    process: Mutex<Option<(Arc<Runner>, String)>>,
}

impl State {
    async fn handle_streaming(
        &self,
        request: ControllerRequest,
        updates: &mpsc::UnboundedSender<ModelStreamEvent>,
    ) -> Result<ControllerResponse> {
        let thread_id = match &request {
            ControllerRequest::ThreadSend { thread_id, .. }
            | ControllerRequest::ThreadContinue { thread_id } => *thread_id,
            _ => unreachable!("non-streaming request dispatched as streaming"),
        };
        let (cancel_requested, _) = watch::channel(false);
        let (cancellation, _) = watch::channel(None);
        let active = Arc::new(ActiveTurn {
            cancel_requested,
            cancellation,
            cancelling: AtomicBool::new(false),
            uncancellable: Mutex::new(()),
            process: Mutex::new(None),
        });
        let mut active_turns = self.active_turns.lock().await;
        if active_turns.contains_key(&thread_id) {
            bail!("thread already has an active turn");
        }
        active_turns.insert(thread_id, Arc::clone(&active));
        drop(active_turns);
        let mut cancel_requested = active.cancel_requested.subscribe();
        let mut cancellation = active.cancellation.subscribe();
        let response = async {
            match request {
                ControllerRequest::ThreadSend { thread_id, message } => {
                    self.run_turn(thread_id, message, Some(updates)).await
                }
                ControllerRequest::ThreadContinue { thread_id } => {
                    self.continue_thread(thread_id, Some(updates)).await
                }
                _ => unreachable!("non-streaming request dispatched as streaming"),
            }
        };
        tokio::pin!(response);
        let mut response = tokio::select! {
            biased;
            changed = cancel_requested.changed() => {
                changed.context("turn cancellation channel closed")?;
                cancellation.changed().await.context("turn cancellation channel closed")?;
                match cancellation.borrow().clone().expect("cancellation completed") {
                    Ok(()) => Ok(ControllerResponse::ThreadCancelled),
                    Err(message) => bail!("{message}"),
                }
            }
            response = &mut response => response,
        };
        if active.cancelling.load(Ordering::Acquire)
            && !matches!(response, Ok(ControllerResponse::ThreadCancelled))
        {
            if !*cancel_requested.borrow() {
                cancel_requested
                    .changed()
                    .await
                    .context("turn cancellation channel closed")?;
            }
            if cancellation.borrow().is_none() {
                cancellation
                    .changed()
                    .await
                    .context("turn cancellation channel closed")?;
            }
            response = match cancellation
                .borrow()
                .clone()
                .expect("cancellation completed")
            {
                Ok(()) => Ok(ControllerResponse::ThreadCancelled),
                Err(message) => Err(anyhow!(message)),
            };
        }
        if !active.cancelling.load(Ordering::Acquire) {
            let mut active_turns = self.active_turns.lock().await;
            if active_turns
                .get(&thread_id)
                .is_some_and(|current| Arc::ptr_eq(current, &active))
            {
                active_turns.remove(&thread_id);
            }
        }
        response
    }

    async fn codex_login_status(&self) -> Result<ControllerResponse> {
        match self.provider.login_status().await {
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
                models: self.provider.models().await?,
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
                let models = self.provider.models().await?;
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
            ControllerRequest::ThreadCheckpointCreate { thread_id } => {
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                self.ensure_no_pending_approval(thread_id).await?;
                let checkpoint_id = self
                    .store
                    .create_checkpoint(thread_id, checkpoint_time_ms(), "manual".to_owned())
                    .await
                    .context("failed to create checkpoint")?;
                Ok(ControllerResponse::ThreadCheckpointCreated { checkpoint_id })
            }
            ControllerRequest::ThreadCheckpointList { thread_id } => {
                let checkpoints = self
                    .store
                    .checkpoints(thread_id)
                    .await
                    .context("failed to list checkpoints")?;
                Ok(ControllerResponse::ThreadCheckpointList { checkpoints })
            }
            ControllerRequest::ThreadCheckpointEvents { checkpoint_id } => {
                let events = self
                    .store
                    .checkpoint_events(checkpoint_id)
                    .await
                    .context("failed to load checkpoint events")?
                    .into_iter()
                    .map(protocol_event)
                    .collect();
                Ok(ControllerResponse::ThreadCheckpointEvents { events })
            }
            ControllerRequest::ThreadCheckpointRestore {
                thread_id,
                checkpoint_id,
            } => {
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                self.ensure_no_pending_approval(thread_id).await?;
                self.store
                    .restore_checkpoint(thread_id, checkpoint_id, checkpoint_time_ms())
                    .await
                    .context("failed to restore checkpoint")?;
                Ok(ControllerResponse::ThreadCheckpointRestored)
            }
            ControllerRequest::ThreadFork {
                thread_id,
                checkpoint_id,
                sequence,
                display_name,
            } => {
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                self.ensure_no_pending_approval(thread_id).await?;
                let thread_id = self
                    .store
                    .fork_thread(thread_id, checkpoint_id, sequence, display_name)
                    .await
                    .context("failed to fork thread")?;
                Ok(ControllerResponse::ThreadForked { thread_id })
            }
            ControllerRequest::ThreadRewind {
                thread_id,
                checkpoint_id,
                sequence,
            } => {
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                self.ensure_no_pending_approval(thread_id).await?;
                self.store
                    .rewind(thread_id, checkpoint_id, sequence, checkpoint_time_ms())
                    .await
                    .context("failed to rewind thread")?;
                Ok(ControllerResponse::ThreadRewound)
            }
            ControllerRequest::ThreadContinue { thread_id } => {
                self.continue_thread(thread_id, None).await
            }
            ControllerRequest::ThreadCancel { thread_id } => self.cancel_thread(thread_id).await,
            ControllerRequest::CodexLogin => {
                codex_login(&self.auth_home).await?;
                self.provider.reload_auth().await;
                self.codex_login_status().await
            }
            ControllerRequest::CodexLogout => {
                self.provider.logout().await?;
                Ok(ControllerResponse::CodexLoggedOut)
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
                background,
                timeout_ms,
                timeout_action,
            } => {
                tracing::debug!(
                    runner,
                    %command,
                    background,
                    ?timeout_ms,
                    ?timeout_action,
                    "executing command"
                );
                let runner = self.runner(&runner).await?;
                runner
                    .request(RunnerRequest::ExecCommand {
                        command,
                        background,
                        timeout_ms,
                        timeout_action,
                        environment: runner.environment.lock().await.clone(),
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

    async fn run_turn(
        &self,
        thread_id: i64,
        message: String,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ControllerResponse> {
        let _guard = self.thread_lock(thread_id).lock_owned().await;
        self.prepare_thread_for_turn(thread_id, updates).await?;
        self.sync_skills(thread_id, updates).await?;
        self.store
            .name_thread_if_unnamed(thread_id, message.clone())
            .await
            .context("failed to name thread")?;
        self.sync_workspace_instructions(thread_id).await?;
        self.append_event(
            thread_id,
            EventKind::UserMessage,
            json!({ "content": message }),
            updates,
        )
        .await
        .context("failed to save user message")?;
        self.continue_turn(thread_id, updates).await
    }

    async fn continue_thread(
        &self,
        thread_id: i64,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ControllerResponse> {
        let _guard = self.thread_lock(thread_id).lock_owned().await;
        self.prepare_thread_for_turn(thread_id, updates).await?;
        self.sync_skills(thread_id, updates).await?;
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load thread history")?;
        let resumable = events.iter().rev().find(|event| {
            matches!(
                event.kind,
                EventKind::UserMessage
                    | EventKind::AssistantMessage
                    | EventKind::ToolCall
                    | EventKind::ToolResult
                    | EventKind::Compaction
            )
        });
        match resumable.map(|event| event.kind) {
            Some(EventKind::UserMessage | EventKind::ToolResult | EventKind::Compaction) => {}
            Some(EventKind::AssistantMessage) => bail!("thread turn is already complete"),
            Some(EventKind::ToolCall) => unreachable!(),
            None => bail!("thread has no resumable history"),
            _ => unreachable!(),
        }
        self.sync_workspace_instructions(thread_id).await?;
        self.continue_turn(thread_id, updates).await
    }

    async fn prepare_thread_for_turn(
        &self,
        thread_id: i64,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<()> {
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load thread history")?;
        let Some(tool_call) = events
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.kind,
                    EventKind::UserMessage
                        | EventKind::AssistantMessage
                        | EventKind::ToolCall
                        | EventKind::ToolResult
                        | EventKind::Compaction
                )
            })
            .filter(|event| event.kind == EventKind::ToolCall)
        else {
            return Ok(());
        };
        self.approvals
            .lock()
            .await
            .retain(|_, approval| approval.thread_id != thread_id);
        self.save_tool_result(
            thread_id,
            tool_call.payload["name"]
                .as_str()
                .context("tool call has no name")?,
            tool_call.payload["call_id"].as_str(),
            ToolOutcome::text("tool execution was interrupted before completion".to_owned()),
            tool_call.payload["type"].as_str() == Some("custom"),
            updates,
        )
        .await
        .context("failed to save interrupted tool result")
    }

    async fn cancel_thread(&self, thread_id: i64) -> Result<ControllerResponse> {
        let active = {
            let active_turns = self.active_turns.lock().await;
            let Some(active) = active_turns.get(&thread_id).cloned() else {
                return Ok(ControllerResponse::ThreadNotActive);
            };
            active.cancelling.store(true, Ordering::Release);
            active
        };
        let result = async {
            let _uncancellable = active.uncancellable.lock().await;
            let mut process = active.process.lock().await;
            active.cancel_requested.send_replace(true);
            if let Some((runner, process_handle)) = process.take() {
                match runner
                    .request_raw(RunnerRequest::StopProcess { process_handle })
                    .await?
                {
                    RunnerResponse::ProcessStopped { .. } => {}
                    RunnerResponse::Error { message } => bail!("{message}"),
                    _ => bail!("runner returned an invalid stop_process response"),
                }
            }
            drop(process);
            let _guard = self.thread_lock(thread_id).lock_owned().await;
            self.approvals
                .lock()
                .await
                .retain(|_, approval| approval.thread_id != thread_id);
            self.prepare_thread_for_turn(thread_id, None).await
        }
        .await;
        let mut active_turns = self.active_turns.lock().await;
        if active_turns
            .get(&thread_id)
            .is_some_and(|current| Arc::ptr_eq(current, &active))
        {
            active_turns.remove(&thread_id);
        }
        drop(active_turns);
        let outcome = result.map_err(|error| format!("{error:#}"));
        active.cancellation.send_replace(Some(outcome.clone()));
        outcome
            .map(|()| ControllerResponse::ThreadCancelled)
            .map_err(anyhow::Error::msg)
    }

    fn thread_lock(&self, thread_id: i64) -> Arc<Mutex<()>> {
        Arc::clone(
            self.thread_locks
                .lock()
                .unwrap()
                .entry(thread_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    async fn ensure_no_pending_approval(&self, thread_id: i64) -> Result<()> {
        if self
            .approvals
            .lock()
            .await
            .values()
            .any(|approval| approval.thread_id == thread_id)
        {
            bail!("thread has a pending approval");
        }
        Ok(())
    }

    async fn continue_turn(
        &self,
        thread_id: i64,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ControllerResponse> {
        let prompt_cache_key = format!(
            "{:x}",
            Sha256::digest(format!("{}-{thread_id}", self.prompt_cache_namespace))
        );
        let model_session = self.provider.start_turn(&prompt_cache_key).await?;
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
            let selected_model = self
                .provider
                .models()
                .await?
                .into_iter()
                .find(|candidate| candidate.id == model);
            let context_window = selected_model
                .as_ref()
                .and_then(|model| model.context_window);
            let auto_compact_token_limit =
                selected_model.and_then(|model| model.auto_compact_token_limit);
            let active_history_start = events
                .iter()
                .rposition(|event| event.kind == EventKind::Compaction)
                .map_or(0, |index| index + 1);
            let active_tokens = events[active_history_start..]
                .iter()
                .rev()
                .find(|event| event.kind == EventKind::TokenUsage)
                .and_then(|event| {
                    event.payload["usage"]["total_tokens"]
                        .as_i64()
                        .or_else(|| event.payload["total_tokens"].as_i64())
                });
            if active_tokens
                .zip(auto_compact_token_limit)
                .is_some_and(|(tokens, limit)| tokens >= limit)
            {
                let request = self.provider.compaction_snapshot(
                    &model,
                    &reasoning_effort,
                    &events,
                    &prompt_cache_key,
                )?;
                self.append_event(
                    thread_id,
                    EventKind::ModelRequest,
                    json!({
                        "kind": "compaction",
                        "started_at_ms": unix_time_ms(),
                        "request": request,
                        "context_window": context_window,
                        "auto_compact_token_limit": auto_compact_token_limit,
                        "compacted": active_history_start > 0,
                    }),
                    updates,
                )
                .await
                .context("failed to save compaction request")?;
                self.store
                    .create_checkpoint(thread_id, checkpoint_time_ms(), "compaction".to_owned())
                    .await
                    .context("failed to checkpoint history before compaction")?;
                let items = model_session
                    .compact(&model, &reasoning_effort, &events, &prompt_cache_key)
                    .await?;
                if !items.is_empty() {
                    let workspace_instructions = workspace_instructions(&events);
                    let workspace_event = if workspace_instructions.tracked {
                        let transition = if workspace_instructions.content.is_some() {
                            "initial"
                        } else {
                            "removal"
                        };
                        Some(json!({
                            "content": workspace_instructions.content,
                            "transition": transition,
                        }))
                    } else {
                        None
                    };
                    self.store
                        .replace_with_compaction(
                            thread_id,
                            json!({ "items": items }),
                            workspace_event,
                            skill_event(&events),
                        )
                        .await
                        .context("failed to replace history after compaction")?;
                    events = self
                        .store
                        .events(thread_id)
                        .await
                        .context("failed to reload compacted model history")?;
                }
            }
            let request = self.provider.completion_snapshot(
                &model,
                &reasoning_effort,
                &events,
                &prompt_cache_key,
            )?;
            let request_sequence = self
                .append_event(
                    thread_id,
                    EventKind::ModelRequest,
                    json!({
                        "kind": "response",
                        "started_at_ms": unix_time_ms(),
                        "request": request,
                        "context_window": context_window,
                        "auto_compact_token_limit": auto_compact_token_limit,
                        "compacted": events.iter().any(|event| event.kind == EventKind::Compaction),
                    }),
                    updates,
                )
                .await
                .context("failed to save model request")?;
            let completion = model_session
                .complete(
                    &model,
                    &reasoning_effort,
                    &events,
                    updates,
                    &prompt_cache_key,
                )
                .await?;
            for item in completion.reasoning {
                self.store
                    .append(thread_id, EventKind::Reasoning, json!({ "item": item }))
                    .await
                    .context("failed to save encrypted reasoning")?;
            }
            if let Some(usage) = completion.token_usage {
                self.append_event(
                    thread_id,
                    EventKind::TokenUsage,
                    json!({
                        "request_sequence": request_sequence,
                        "usage": usage,
                    }),
                    updates,
                )
                .await
                .context("failed to save token usage")?;
            }
            if !completion.rate_limits.is_empty() {
                self.append_event(
                    thread_id,
                    EventKind::RateLimits,
                    json!({
                        "request_sequence": request_sequence,
                        "snapshots": completion.rate_limits,
                    }),
                    updates,
                )
                .await
                .context("failed to save rate limits")?;
            }

            if let Some(response) = self
                .execute_model_responses(thread_id, completion.responses.into(), updates)
                .await?
            {
                return Ok(response);
            }
        }
    }

    async fn sync_workspace_instructions(&self, thread_id: i64) -> Result<()> {
        let content = self.read_workspace_instructions().await?;
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load workspace instruction state")?;
        let previous = workspace_instructions(&events);
        if previous.tracked && previous.content == content {
            return Ok(());
        }
        if !previous.tracked && content.is_none() {
            return Ok(());
        }

        let transition = match (&previous.content, &content) {
            (_, None) => "removal",
            (Some(_), Some(_)) => "replacement",
            (None, Some(_)) => "initial",
        };
        self.store
            .append(
                thread_id,
                EventKind::WorkspaceInstructions,
                json!({
                    "content": content,
                    "transition": transition,
                }),
            )
            .await
            .context("failed to save workspace instructions")?;
        Ok(())
    }

    async fn sync_skills(
        &self,
        thread_id: i64,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<()> {
        let generation = self.collect_skill_generation().await?;

        let runners = self.runners.lock().await;
        for (name, runner) in runners.iter() {
            runner
                .sync_skills(&self.skill_store, &generation)
                .await
                .with_context(|| format!("failed to synchronize skills to runner {name}"))?;
        }
        drop(runners);
        *self.skill_generation.lock().await = Some(Arc::clone(&generation));

        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load skill state")?;
        let previous = current_skills(&events);
        if previous.tracked && previous.content == generation.prompt {
            return Ok(());
        }
        if !previous.tracked && generation.prompt.is_none() {
            return Ok(());
        }
        let transition = match (&previous.content, &generation.prompt) {
            (_, None) => "removal",
            (Some(_), Some(_)) => "replacement",
            (None, Some(_)) => "initial",
        };
        self.append_event(
            thread_id,
            EventKind::Skills,
            json!({
                "content": generation.prompt,
                "transition": transition,
            }),
            updates,
        )
        .await
        .context("failed to save skills")?;
        Ok(())
    }

    async fn collect_skill_generation(&self) -> Result<Arc<skills::SkillGeneration>> {
        let workspace = self.workspace.clone();
        let data_home = self.data_home.clone();
        let store = self.skill_store.clone();
        Ok(Arc::new(
            tokio::task::spawn_blocking(move || skills::collect(&workspace, &data_home, &store))
                .await
                .context("skill collection task failed")??,
        ))
    }

    async fn read_workspace_instructions(&self) -> Result<Option<String>> {
        for filename in ["AGENTS.override.md", "AGENTS.md"] {
            let path = self.workspace.join(filename);
            match tokio::fs::metadata(&path).await {
                Ok(metadata) if !metadata.is_file() => continue,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to inspect workspace instructions {}",
                            path.display()
                        )
                    });
                }
            }
            let mut data = match tokio::fs::read(&path).await {
                Ok(data) => data,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to read workspace instructions {}", path.display())
                    });
                }
            };
            data.truncate(WORKSPACE_INSTRUCTIONS_MAX_BYTES);
            let content = String::from_utf8_lossy(&data).trim().to_owned();
            return Ok((!content.is_empty()).then_some(content));
        }
        Ok(None)
    }

    async fn execute_model_responses(
        &self,
        thread_id: i64,
        mut responses: VecDeque<ModelResponse>,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<Option<ControllerResponse>> {
        while let Some(response) = responses.pop_front() {
            match response {
                ModelResponse::AssistantMessage { content } => {
                    self.append_event(
                        thread_id,
                        EventKind::AssistantMessage,
                        json!({ "content": content }),
                        updates,
                    )
                    .await
                    .context("failed to save assistant message")?;
                    return Ok(Some(ControllerResponse::TurnCompleted { content }));
                }
                ModelResponse::WebSearch { item } => {
                    self.append_event(
                        thread_id,
                        EventKind::WebSearch,
                        json!({ "item": item }),
                        updates,
                    )
                    .await
                    .context("failed to save web search")?;
                }
                ModelResponse::ToolCall {
                    name,
                    arguments,
                    call_id,
                } => {
                    self.append_event(
                        thread_id,
                        EventKind::ToolCall,
                        json!({
                            "name": &name,
                            "arguments": &arguments,
                            "call_id": &call_id,
                        }),
                        updates,
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
                                    false,
                                    updates,
                                )
                                .await?
                            {
                                return Ok(Some(response));
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
                                    false,
                                    updates,
                                )
                                .await?
                            {
                                return Ok(Some(response));
                            }
                        }
                        "list_runners" => {
                            let runners = self.list_runners().await?;
                            let result = if runners.is_empty() {
                                "No runners are available.".to_owned()
                            } else {
                                format!(
                                    "Available runners:\n{}",
                                    runners
                                        .iter()
                                        .map(|runner| {
                                            format!("- {}: {}", runner.name, runner.description)
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                )
                            };
                            self.save_tool_result(
                                thread_id,
                                &name,
                                call_id.as_deref(),
                                ToolOutcome::with_artifact(
                                    result,
                                    "runner_list",
                                    json!({ "runners": runners }),
                                ),
                                false,
                                updates,
                            )
                            .await?;
                        }
                        "wait_process" => {
                            let arguments: WaitProcessArguments = serde_json::from_value(arguments)
                                .context("model returned invalid wait_process arguments")?;
                            if let Some(response) = self
                                .route_tool(
                                    thread_id,
                                    name,
                                    call_id,
                                    ToolArguments::WaitProcess(arguments),
                                    false,
                                    updates,
                                )
                                .await?
                            {
                                return Ok(Some(response));
                            }
                        }
                        "write_process" => {
                            let arguments: WriteProcessArguments =
                                serde_json::from_value(arguments)
                                    .context("model returned invalid write_process arguments")?;
                            if let Some(response) = self
                                .route_tool(
                                    thread_id,
                                    name,
                                    call_id,
                                    ToolArguments::WriteProcess(arguments),
                                    false,
                                    updates,
                                )
                                .await?
                            {
                                return Ok(Some(response));
                            }
                        }
                        "stop_process" => {
                            let arguments: StopProcessArguments = serde_json::from_value(arguments)
                                .context("model returned invalid stop_process arguments")?;
                            if let Some(response) = self
                                .route_tool(
                                    thread_id,
                                    name,
                                    call_id,
                                    ToolArguments::StopProcess(arguments),
                                    false,
                                    updates,
                                )
                                .await?
                            {
                                return Ok(Some(response));
                            }
                        }
                        _ => bail!("model requested unsupported tool {name}"),
                    }
                }
                ModelResponse::CustomToolCall {
                    item_id,
                    name,
                    input,
                    call_id,
                } => {
                    if name != "apply_patch" {
                        bail!("model requested unsupported custom tool {name}");
                    }
                    self.append_event(
                        thread_id,
                        EventKind::ToolCall,
                        json!({
                            "type": "custom",
                            "item_id": &item_id,
                            "name": &name,
                            "input": &input,
                            "call_id": &call_id,
                        }),
                        updates,
                    )
                    .await
                    .context("failed to save tool call")?;
                    let arguments = parse_apply_patch_input(input)?;
                    if let Some(response) = self
                        .route_tool(
                            thread_id,
                            name,
                            Some(call_id),
                            ToolArguments::ApplyPatch(arguments),
                            true,
                            updates,
                        )
                        .await?
                    {
                        return Ok(Some(response));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn route_tool(
        &self,
        thread_id: i64,
        name: String,
        call_id: Option<String>,
        arguments: ToolArguments,
        custom: bool,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<Option<ControllerResponse>> {
        let runner = self.runner(arguments.runner()).await?;
        let decision = if runner.approval().await == ApprovalPolicy::Ask {
            let approval_id = self.next_approval_id.fetch_add(1, Ordering::Relaxed) + 1;
            let arguments_json =
                serde_json::to_value(&arguments).context("failed to encode approval arguments")?;
            let (decision, approval) = oneshot::channel();
            self.approvals.lock().await.insert(
                approval_id,
                PendingApproval {
                    thread_id,
                    decision,
                },
            );
            updates
                .context("approval requires a streaming turn")?
                .send(ModelStreamEvent::ApprovalRequired {
                    approval_id,
                    thread_id,
                    tool: name.clone(),
                    arguments: arguments_json,
                })
                .context("turn stream closed while waiting for approval")?;
            Some(
                approval
                    .await
                    .context("approval was removed before it was resolved")?,
            )
        } else {
            None
        };
        let allowed = decision.as_ref().is_none_or(|decision| decision.allowed);
        let active = if allowed && matches!(&arguments, ToolArguments::ApplyPatch(_)) {
            Some(
                self.active_turns
                    .lock()
                    .await
                    .get(&thread_id)
                    .cloned()
                    .context("thread has no active turn")?,
            )
        } else {
            None
        };
        let _uncancellable = match &active {
            Some(active) => Some(active.uncancellable.lock().await),
            None => None,
        };
        let result = if allowed {
            self.execute(thread_id, arguments).await?
        } else {
            let reason = decision.and_then(|decision| decision.reason);
            let output = match reason {
                Some(reason) => format!("user denied the tool call: {reason}"),
                None => "user denied the tool call".to_owned(),
            };
            ToolOutcome::text(output)
        };
        self.save_tool_result(
            thread_id,
            &name,
            call_id.as_deref(),
            result,
            custom,
            updates,
        )
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
        pending
            .decision
            .send(ApprovalDecision { allowed, reason })
            .map_err(|_| anyhow!("turn ended before approval {approval_id} was resolved"))?;
        Ok(ControllerResponse::ApprovalResolved)
    }

    async fn execute(&self, thread_id: i64, arguments: ToolArguments) -> Result<ToolOutcome> {
        match arguments {
            ToolArguments::ExecCommand(arguments) => {
                let runner_name = arguments.runner.clone();
                let runner = self.runner(&arguments.runner).await?;
                let response = if arguments.background {
                    runner
                        .request_raw(RunnerRequest::ExecCommand {
                            command: arguments.command,
                            background: true,
                            timeout_ms: arguments.timeout_ms,
                            timeout_action: arguments.timeout_action,
                            environment: runner.environment.lock().await.clone(),
                        })
                        .await?
                } else {
                    let active = self
                        .active_turns
                        .lock()
                        .await
                        .get(&thread_id)
                        .cloned()
                        .context("thread has no active turn")?;
                    let mut process = active.process.lock().await;
                    let response = runner
                        .request_raw(RunnerRequest::StartCommand {
                            command: arguments.command,
                            environment: runner.environment.lock().await.clone(),
                        })
                        .await?;
                    let RunnerResponse::ProcessStarted { process_handle } = response else {
                        bail!("runner returned an invalid start command response");
                    };
                    *process = Some((Arc::clone(&runner), process_handle.clone()));
                    drop(process);
                    let deadline = arguments
                        .timeout_ms
                        .map(|timeout| Instant::now() + std::time::Duration::from_millis(timeout));
                    let mut collected = None;
                    let response = loop {
                        let timeout_ms = deadline.map_or(1000, |deadline| {
                            deadline
                                .saturating_duration_since(Instant::now())
                                .as_millis()
                                .min(1000)
                                .try_into()
                                .unwrap_or(1000)
                        });
                        match runner
                            .request_raw(RunnerRequest::WaitProcess {
                                process_handle: process_handle.clone(),
                                timeout_ms,
                            })
                            .await?
                        {
                            RunnerResponse::ProcessRunning { output, .. } => {
                                append_command_output(&mut collected, output);
                                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                                    break match arguments.timeout_action {
                                        TimeoutAction::ReturnRunning => {
                                            RunnerResponse::ProcessRunning {
                                                process_handle: process_handle.clone(),
                                                output: collected.take().unwrap(),
                                            }
                                        }
                                        TimeoutAction::Terminate => match runner
                                            .request_raw(RunnerRequest::StopProcess {
                                                process_handle: process_handle.clone(),
                                            })
                                            .await?
                                        {
                                            RunnerResponse::ProcessStopped { output } => {
                                                append_command_output(&mut collected, output);
                                                RunnerResponse::ProcessTimedOut {
                                                    output: collected.take().unwrap(),
                                                }
                                            }
                                            response => response,
                                        },
                                    };
                                }
                            }
                            RunnerResponse::ProcessFinished { output, exit_code } => {
                                append_command_output(&mut collected, output);
                                break RunnerResponse::ProcessFinished {
                                    output: collected.take().unwrap(),
                                    exit_code,
                                };
                            }
                            response => break response,
                        }
                    };
                    active.process.lock().await.take();
                    response
                };
                let artifact = command_artifact(&response)?;
                Ok(ToolOutcome::with_artifact(
                    format_exec_response(&runner_name, response)?,
                    "command_execution",
                    artifact,
                ))
            }
            ToolArguments::ApplyPatch(arguments) => {
                let response = self
                    .runner(&arguments.runner)
                    .await?
                    .request_raw(RunnerRequest::ApplyPatch {
                        patch: arguments.patch,
                    })
                    .await?;
                Ok(match response {
                    RunnerResponse::PatchCompleted { result } => {
                        let output = format_patch_result(&result);
                        ToolOutcome::with_optional_artifact(
                            output,
                            serde_json::to_value(result)
                                .ok()
                                .map(|data| ("patch_operations", data)),
                        )
                    }
                    RunnerResponse::Error { message } => bail!("{message}"),
                    _ => bail!("runner returned an invalid apply_patch response"),
                })
            }
            ToolArguments::WaitProcess(arguments) => {
                let runner_name = arguments.runner.clone();
                let response = self
                    .runner(&arguments.runner)
                    .await?
                    .request_raw(RunnerRequest::WaitProcess {
                        process_handle: arguments.process_handle,
                        timeout_ms: arguments.timeout_ms,
                    })
                    .await?;
                let artifact = command_artifact(&response)?;
                Ok(ToolOutcome::with_artifact(
                    format_process_response("wait_process", &runner_name, response)?,
                    "command_execution",
                    artifact,
                ))
            }
            ToolArguments::WriteProcess(arguments) => {
                let input = arguments.input.into_bytes();
                let artifact = json!({
                    "process_handle": arguments.process_handle,
                    "bytes_written": input.len(),
                });
                let response = self
                    .runner(&arguments.runner)
                    .await?
                    .request_raw(RunnerRequest::WriteProcess {
                        process_handle: arguments.process_handle,
                        input,
                    })
                    .await?;
                Ok(match response {
                    RunnerResponse::InputWritten => ToolOutcome::with_artifact(
                        "Input written.".to_owned(),
                        "process_input",
                        artifact,
                    ),
                    RunnerResponse::Error { message } => bail!("{message}"),
                    _ => bail!("runner returned an invalid write_process response"),
                })
            }
            ToolArguments::StopProcess(arguments) => {
                let runner_name = arguments.runner.clone();
                let response = self
                    .runner(&arguments.runner)
                    .await?
                    .request_raw(RunnerRequest::StopProcess {
                        process_handle: arguments.process_handle,
                    })
                    .await?;
                let artifact = command_artifact(&response)?;
                Ok(ToolOutcome::with_artifact(
                    format_process_response("stop_process", &runner_name, response)?,
                    "command_execution",
                    artifact,
                ))
            }
        }
    }

    async fn save_tool_result(
        &self,
        thread_id: i64,
        name: &str,
        call_id: Option<&str>,
        outcome: ToolOutcome,
        custom: bool,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<()> {
        self.append_event(
            thread_id,
            EventKind::ToolResult,
            json!({
                "type": custom.then_some("custom"),
                "name": name,
                "call_id": call_id,
                "result": outcome.result,
                "artifacts": outcome.artifacts,
            }),
            updates,
        )
        .await
        .context("failed to save tool result")?;
        Ok(())
    }

    async fn append_event(
        &self,
        thread_id: i64,
        kind: EventKind,
        payload: serde_json::Value,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> tokio_rusqlite::Result<i64> {
        let sequence = self.store.append(thread_id, kind, payload.clone()).await?;
        if let Some(updates) = updates {
            updates
                .send(ModelStreamEvent::ThreadEvent(ThreadEvent {
                    sequence,
                    kind: kind.as_str().to_owned(),
                    payload,
                }))
                .ok();
        }
        Ok(sequence)
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
            Runner::start(&name, description, approval, command, self.platform.clone()).await?,
        );
        let cached_generation = self.skill_generation.lock().await.clone();
        let generation = match cached_generation {
            Some(generation) => generation,
            None => {
                let generation = self.collect_skill_generation().await?;
                *self.skill_generation.lock().await = Some(Arc::clone(&generation));
                generation
            }
        };
        runner.sync_skills(&self.skill_store, &generation).await?;
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

fn workspace_instructions(events: &[storage::Event]) -> WorkspaceInstructions {
    events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::WorkspaceInstructions)
        .map_or(
            WorkspaceInstructions {
                content: None,
                tracked: false,
            },
            |event| WorkspaceInstructions {
                content: event.payload["content"].as_str().map(str::to_owned),
                tracked: true,
            },
        )
}

fn current_skills(events: &[storage::Event]) -> WorkspaceInstructions {
    events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::Skills)
        .map_or(
            WorkspaceInstructions {
                content: None,
                tracked: false,
            },
            |event| WorkspaceInstructions {
                content: event.payload["content"].as_str().map(str::to_owned),
                tracked: true,
            },
        )
}

fn skill_event(events: &[storage::Event]) -> Option<serde_json::Value> {
    let skills = current_skills(events);
    skills.tracked.then(|| {
        json!({
            "content": skills.content,
            "transition": if skills.content.is_some() { "initial" } else { "removal" },
        })
    })
}

#[derive(Deserialize, serde::Serialize)]
struct ExecCommandArguments {
    runner: String,
    command: String,
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
}

#[derive(Deserialize, serde::Serialize)]
struct WaitProcessArguments {
    runner: String,
    process_handle: String,
    timeout_ms: u64,
}

#[derive(Deserialize, serde::Serialize)]
struct WriteProcessArguments {
    runner: String,
    process_handle: String,
    input: String,
}

#[derive(Deserialize, serde::Serialize)]
struct StopProcessArguments {
    runner: String,
    process_handle: String,
}

fn parse_apply_patch_input(patch: String) -> Result<ApplyPatchArguments> {
    let mut lines = patch.lines();
    if lines.next() != Some("*** Begin Patch") {
        bail!("custom apply_patch input must start with '*** Begin Patch'");
    }
    let runner = lines
        .next()
        .and_then(|line| line.strip_prefix("*** Runner: "))
        .filter(|runner| !runner.is_empty())
        .context("custom apply_patch input must include a non-empty Runner")?
        .to_owned();
    Ok(ApplyPatchArguments { runner, patch })
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum ToolArguments {
    ExecCommand(ExecCommandArguments),
    ApplyPatch(ApplyPatchArguments),
    WaitProcess(WaitProcessArguments),
    WriteProcess(WriteProcessArguments),
    StopProcess(StopProcessArguments),
}

struct ToolOutcome {
    result: serde_json::Value,
    artifacts: Vec<serde_json::Value>,
}

impl ToolOutcome {
    fn text(result: String) -> Self {
        Self {
            result: serde_json::Value::String(result),
            artifacts: Vec::new(),
        }
    }

    fn with_artifact(result: String, kind: &str, data: serde_json::Value) -> Self {
        Self {
            result: serde_json::Value::String(result),
            artifacts: vec![json!({
                "kind": kind,
                "data": data,
            })],
        }
    }

    fn with_optional_artifact(result: String, artifact: Option<(&str, serde_json::Value)>) -> Self {
        match artifact {
            Some((kind, data)) => Self::with_artifact(result, kind, data),
            None => Self::text(result),
        }
    }
}

impl ToolArguments {
    fn runner(&self) -> &str {
        match self {
            Self::ExecCommand(arguments) => &arguments.runner,
            Self::ApplyPatch(arguments) => &arguments.runner,
            Self::WaitProcess(arguments) => &arguments.runner,
            Self::WriteProcess(arguments) => &arguments.runner,
            Self::StopProcess(arguments) => &arguments.runner,
        }
    }
}

struct PendingApproval {
    thread_id: i64,
    decision: oneshot::Sender<ApprovalDecision>,
}

struct ApprovalDecision {
    allowed: bool,
    reason: Option<String>,
}

fn return_running() -> TimeoutAction {
    TimeoutAction::ReturnRunning
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is before the Unix epoch")
        .as_millis()
}

fn checkpoint_time_ms() -> i64 {
    i64::try_from(unix_time_ms()).expect("current time fits in i64 milliseconds")
}

fn protocol_event(event: storage::Event) -> ThreadEvent {
    ThreadEvent {
        sequence: event.sequence,
        kind: event.kind.as_str().to_owned(),
        payload: event.payload,
    }
}

struct Runner {
    config: Mutex<RunnerConfig>,
    child: Mutex<Child>,
    client: RunnerClient,
    environment: Mutex<CommandEnvironment>,
    skill_digest: Mutex<Option<String>>,
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
        platform: Option<Arc<PlatformStore>>,
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
        match client.request_raw(RunnerRequest::Initialize).await? {
            RunnerResponse::Ready => {}
            response => bail!("runner {name} returned an invalid readiness response: {response:?}"),
        }
        let mut environment = CommandEnvironment::default();
        if let Some(platform) = platform {
            let tools = platform.tools()?;
            let path = deploy_tree(&client, TreeObjects::Platform(platform), tools).await?;
            environment.prepend_path.push(format!("{path}/bin"));
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
            environment: Mutex::new(environment),
            skill_digest: Mutex::new(None),
        })
    }

    async fn request(&self, request: RunnerRequest) -> Result<ControllerResponse> {
        map_runner_response(self.client.request_raw(request).await?)
    }

    async fn request_raw(&self, request: RunnerRequest) -> Result<RunnerResponse> {
        self.client.request_raw(request).await
    }

    async fn approval(&self) -> ApprovalPolicy {
        self.config.lock().await.approval
    }

    async fn sync_skills(
        &self,
        store: &AtraStore,
        generation: &skills::SkillGeneration,
    ) -> Result<()> {
        let digest = generation.manifest.digest();
        if self.skill_digest.lock().await.as_deref() == Some(&digest) {
            return Ok(());
        }
        let mut environment = self.environment.lock().await;
        if generation.manifest.entries.is_empty() {
            environment.set.remove("ATRA_SKILLS");
        } else {
            let path = deploy_tree(
                &self.client,
                TreeObjects::Store(store.clone()),
                generation.manifest.clone(),
            )
            .await?;
            environment
                .set
                .insert("ATRA_SKILLS".to_owned(), format!("{path}/skills"));
        }
        *self.skill_digest.lock().await = Some(digest);
        Ok(())
    }
}

#[derive(Clone)]
enum TreeObjects {
    Platform(Arc<PlatformStore>),
    Store(AtraStore),
}

async fn deploy_tree(
    client: &RunnerClient,
    objects: TreeObjects,
    manifest: TreeManifest,
) -> Result<String> {
    let expected_digest = manifest.digest();
    loop {
        match client
            .request_raw(RunnerRequest::PrepareTree {
                manifest: manifest.clone(),
            })
            .await?
        {
            RunnerResponse::MissingObjects { digests } => {
                for digest in digests {
                    let objects = objects.clone();
                    let object_digest = digest.clone();
                    let (compressed, executable) = tokio::task::spawn_blocking(move || {
                        let mut encoder = zstd::Encoder::new(Vec::new(), 3)
                            .context("failed to compress object")?;
                        let executable = match objects {
                            TreeObjects::Platform(platform) => {
                                platform.copy_object_to(&object_digest, &mut encoder)?
                            }
                            TreeObjects::Store(store) => {
                                store.copy_object_to(&object_digest, &mut encoder)?
                            }
                        };
                        let compressed = encoder.finish().context("failed to finish object")?;
                        Ok::<_, anyhow::Error>((compressed, executable))
                    })
                    .await
                    .context("object compression task failed")??;
                    match client
                        .request_raw(RunnerRequest::UploadObject {
                            digest,
                            executable,
                            blob: STANDARD.encode(compressed),
                        })
                        .await?
                    {
                        RunnerResponse::ObjectStored => {}
                        response => {
                            bail!("runner returned an invalid object response: {response:?}")
                        }
                    }
                }
            }
            RunnerResponse::TreeReady { digest, path } => {
                if digest != expected_digest {
                    bail!("runner returned tree digest {digest}, expected {expected_digest}");
                }
                return Ok(path);
            }
            response => bail!("runner returned an invalid tree response: {response:?}"),
        }
    }
}

struct RunnerClient {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<RunnerResponse>>>>,
    next_request_id: Arc<AtomicU64>,
    name: String,
}

impl RunnerClient {
    fn new(stdin: ChildStdin, stdout: tokio::process::ChildStdout, name: &str) -> Self {
        let stdin = Arc::new(Mutex::new(stdin));
        let reader_stdin = Arc::clone(&stdin);
        let pending = Arc::new(StdMutex::new(
            HashMap::<u64, oneshot::Sender<RunnerResponse>>::new(),
        ));
        let reader_pending = Arc::clone(&pending);
        let next_request_id = Arc::new(AtomicU64::new(0));
        let reader_next_request_id = Arc::clone(&next_request_id);
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
                            let sender =
                                reader_pending.lock().unwrap().remove(&envelope.request_id);
                            if let Some(sender) = sender {
                                if let Err(RunnerResponse::ProcessStarted { process_handle }) =
                                    sender.send(envelope.response)
                                {
                                    let request_id =
                                        reader_next_request_id.fetch_add(1, Ordering::Relaxed);
                                    let mut request = serde_json::to_vec(&RunnerRequestEnvelope {
                                        request_id,
                                        request: RunnerRequest::StopProcess { process_handle },
                                    })
                                    .expect("runner stop request should encode");
                                    request.push(b'\n');
                                    if let Err(error) =
                                        reader_stdin.lock().await.write_all(&request).await
                                    {
                                        tracing::warn!(
                                            runner = runner_name,
                                            %error,
                                            "failed to stop an abandoned process"
                                        );
                                    }
                                }
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
            stdin,
            pending,
            next_request_id,
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
        RunnerResponse::MissingObjects { .. }
        | RunnerResponse::TreeReady { .. }
        | RunnerResponse::ObjectStored => {
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
            output: format_command_output(&output),
        }),
        RunnerResponse::ProcessFinished { output, exit_code } => {
            tracing::info!(
                ?exit_code,
                output_bytes = output.content.len(),
                "process finished"
            );
            Ok(ControllerResponse::ProcessFinished {
                output: format_command_output(&output),
                exit_code,
            })
        }
        RunnerResponse::ProcessTimedOut { output } => Ok(ControllerResponse::ProcessTimedOut {
            output: format_command_output(&output),
        }),
        RunnerResponse::InputWritten => Ok(ControllerResponse::InputWritten),
        RunnerResponse::ProcessStopped { output } => Ok(ControllerResponse::ProcessStopped {
            output: format_command_output(&output),
        }),
        RunnerResponse::PatchCompleted { .. } => {
            bail!("runner returned an unexpected patch response")
        }
        RunnerResponse::Error { message } => bail!("{message}"),
    }
}

fn format_exec_response(runner: &str, response: RunnerResponse) -> Result<String> {
    match response {
        RunnerResponse::ProcessStarted { process_handle } => {
            Ok(format!("Process started with handle {process_handle}"))
        }
        RunnerResponse::ProcessRunning {
            process_handle,
            output,
        } => Ok(append_process_status(
            model_command_output(&output, runner),
            &format!("Process {process_handle} is still running"),
        )),
        RunnerResponse::ProcessFinished {
            output,
            exit_code: Some(0),
        } => {
            let output = model_command_output(&output, runner);
            Ok(if output.is_empty() {
                "Process completed with no output".to_owned()
            } else {
                output
            })
        }
        RunnerResponse::ProcessFinished { output, exit_code } => {
            let exit_code = exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned());
            Ok(append_process_status(
                model_command_output(&output, runner),
                &format!("Process exited with code {exit_code}"),
            ))
        }
        RunnerResponse::ProcessTimedOut { output } => Ok(append_process_status(
            model_command_output(&output, runner),
            "Process timed out",
        )),
        RunnerResponse::Error { message } => bail!("{message}"),
        RunnerResponse::Ready
        | RunnerResponse::MissingObjects { .. }
        | RunnerResponse::TreeReady { .. }
        | RunnerResponse::ObjectStored
        | RunnerResponse::InputWritten
        | RunnerResponse::ProcessStopped { .. }
        | RunnerResponse::PatchCompleted { .. } => {
            bail!("runner returned an invalid tool response")
        }
    }
}

fn format_patch_result(result: &ApplyPatchResult) -> String {
    let results = match result {
        ApplyPatchResult::ParseError { error } => {
            return format!("apply_patch failed:\n{error}");
        }
        ApplyPatchResult::Operations { results } => results,
    };
    let failed = results.iter().any(|result| {
        matches!(
            result,
            PatchOperationResult::Added {
                outcome: PatchOperationOutcome::Failed { .. },
                ..
            } | PatchOperationResult::Deleted {
                outcome: PatchOperationOutcome::Failed { .. },
                ..
            } | PatchOperationResult::Updated {
                outcome: PatchOperationOutcome::Failed { .. },
                ..
            } | PatchOperationResult::Moved {
                outcome: PatchOperationOutcome::Failed { .. },
                ..
            }
        )
    });
    let mut output = if failed {
        String::from("apply_patch completed with errors:\n")
    } else {
        String::from("Success. Updated the following files:\n")
    };
    for result in results {
        let (label, outcome) = match result {
            PatchOperationResult::Added { path, outcome } => {
                (format!("A {}", path.display()), outcome)
            }
            PatchOperationResult::Deleted { path, outcome } => {
                (format!("D {}", path.display()), outcome)
            }
            PatchOperationResult::Updated { path, outcome } => {
                (format!("M {}", path.display()), outcome)
            }
            PatchOperationResult::Moved { from, to, outcome } => {
                (format!("R {} -> {}", from.display(), to.display()), outcome)
            }
        };
        match outcome {
            PatchOperationOutcome::Applied { .. } => output.push_str(&format!("{label}\n")),
            PatchOperationOutcome::Failed { error } => {
                output.push_str(&format!("{label}: {error}\n"));
            }
        }
    }
    output
}

fn command_artifact(response: &RunnerResponse) -> Result<serde_json::Value> {
    Ok(match response {
        RunnerResponse::ProcessStarted { .. } => json!({
            "state": "started",
        }),
        RunnerResponse::ProcessRunning { output, .. } => json!({
            "state": "running",
            "output": format_command_output(output),
        }),
        RunnerResponse::ProcessFinished { output, exit_code } => json!({
            "state": "finished",
            "output": format_command_output(output),
            "exit_code": exit_code,
        }),
        RunnerResponse::ProcessTimedOut { output } => json!({
            "state": "timed_out",
            "output": format_command_output(output),
        }),
        RunnerResponse::ProcessStopped { output } => json!({
            "state": "stopped",
            "output": format_command_output(output),
        }),
        RunnerResponse::Error { message } => bail!("{message}"),
        _ => bail!("runner returned an invalid command response"),
    })
}

fn format_process_response(tool: &str, runner: &str, response: RunnerResponse) -> Result<String> {
    match response {
        RunnerResponse::ProcessRunning {
            process_handle,
            output,
        } => Ok(append_process_status(
            model_command_output(&output, runner),
            &format!("Process {process_handle} is still running"),
        )),
        RunnerResponse::ProcessFinished {
            output,
            exit_code: Some(0),
        } => {
            let output = model_command_output(&output, runner);
            Ok(if output.is_empty() {
                "Process completed with no output".to_owned()
            } else {
                output
            })
        }
        RunnerResponse::ProcessFinished { output, exit_code } => Ok(append_process_status(
            model_command_output(&output, runner),
            &format!(
                "Process exited with code {}",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
        )),
        RunnerResponse::ProcessStopped { output } => Ok(append_process_status(
            model_command_output(&output, runner),
            "Process stopped",
        )),
        RunnerResponse::Error { message } => bail!("{message}"),
        _ => bail!("runner returned an invalid {tool} response"),
    }
}

fn append_command_output(collected: &mut Option<CommandOutput>, output: CommandOutput) {
    match collected {
        Some(collected) => {
            collected.content.push_str(&output.content);
            collected.omitted_bytes += output.omitted_bytes;
            collected.full_output_path = output.full_output_path;
            if collected.content.len() > MAX_TOOL_OUTPUT_BYTES {
                let head_end = floor_char_boundary(&collected.content, MAX_TOOL_OUTPUT_BYTES / 2);
                let tail_start = ceil_char_boundary(
                    &collected.content,
                    collected.content.len() - MAX_TOOL_OUTPUT_BYTES / 2,
                )
                .max(head_end);
                collected.omitted_bytes += tail_start - head_end;
                collected.content.replace_range(head_end..tail_start, "");
            }
        }
        None => *collected = Some(output),
    }
}

const MAX_TOOL_OUTPUT_BYTES: usize = 40_000;

fn format_command_output(output: &CommandOutput) -> String {
    format_command_output_with_location(output, &output.full_output_path.display().to_string())
}

fn format_command_output_with_location(
    output: &CommandOutput,
    full_output_location: &str,
) -> String {
    if output.omitted_bytes == 0 && output.content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output.content.clone();
    }

    let head_end = floor_char_boundary(
        &output.content,
        (MAX_TOOL_OUTPUT_BYTES / 2).min(output.content.len()),
    );
    let tail_start = ceil_char_boundary(
        &output.content,
        output
            .content
            .len()
            .saturating_sub(MAX_TOOL_OUTPUT_BYTES - MAX_TOOL_OUTPUT_BYTES / 2),
    )
    .max(head_end);
    let omitted_bytes = output.omitted_bytes + tail_start.saturating_sub(head_end);
    format!(
        "{}\n\n... {omitted_bytes} bytes omitted; full output: {full_output_location} ...\n\n{}",
        &output.content[..head_end],
        &output.content[tail_start..]
    )
}

fn model_command_output(output: &CommandOutput, runner: &str) -> String {
    let location = format!("runner \"{runner}\": {}", output.full_output_path.display());
    let output = format_command_output_with_location(output, &location);
    output
        .chars()
        .filter(|character| {
            matches!(character, '\t' | '\n' | '\r')
                || (*character >= ' ' && !('\u{fff9}'..='\u{fffb}').contains(character))
        })
        .filter(|character| *character != '\r')
        .collect()
}

fn append_process_status(output: String, status: &str) -> String {
    if output.is_empty() {
        status.to_owned()
    } else {
        format!("{output}\n\n{status}")
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn current_platform() -> Result<Option<PlatformStore>> {
    let platform = match env::consts::ARCH {
        "x86_64" => "x86_64-linux-static",
        "aarch64" => "aarch64-linux-static",
        _ => return Ok(None),
    };
    let root = xdg::BaseDirectories::new()
        .get_data_home()
        .context("cannot determine the XDG data directory")?
        .join("atra");
    PlatformStore::load(root, platform)
}

struct SocketGuard<'a>(&'a Path);

impl Drop for SocketGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
