use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use atra_patch::{ApplyPatchResult, PatchOperationOutcome, PatchOperationResult};
use atra_platform::PlatformStore;
use atra_protocol::{
    ApprovalPolicy, BackgroundProcess, BackgroundProcessDetail, CommandEnvironment,
    CommandExecutionArtifact, CommandMode, CommandOutput, CompactionEvent, ControllerRequest,
    ControllerResponse, CustomToolType, FrozenBoundaryEvent, InstructionEvent,
    InstructionTransition, ItemEvent, MessageEvent, ModelRequestEvent, ModelRequestKind,
    ProcessStatus, RateLimitsEvent, Runner as RunnerInfo, RunnerOperationArtifact,
    RunnerOperationUpdate, RunnerRequest, RunnerResponse, RunnersEvent, ThreadEvent,
    ThreadEventData, TokenUsageEvent, ToolArtifact, ToolCallEvent, ToolResultEvent,
};
use atra_store::{Store as AtraStore, TreeManifest};
use base64::{Engine, engine::general_purpose::STANDARD};
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_login::{
    AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthRouteConfig, CLIENT_ID, ServerOptions,
    default_client::set_default_originator, logout_with_revoke, run_login_server,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{Mutex, mpsc, watch},
    time::Instant,
};

mod connection;
mod lifecycle;
mod model;
mod runner_client;
mod skills;
mod storage;

use lifecycle::{ApprovalDecision, TurnLifecycle};
use model::{DEFAULT_MODEL, ModelResponse, ModelStreamEvent, Provider};
use runner_client::{PrepareTreeResult, RunnerClient};
use storage::Store;

const WORKSPACE_INSTRUCTIONS_MAX_BYTES: usize = 32 * 1024;
const ACTIVE_CONTEXT_HIGH_TOKENS: usize = 96_000;
const ACTIVE_CONTEXT_LOW_TOKENS: usize = 48_000;
const MINIMUM_FULL_RESULT_REQUESTS: usize = 3;
const MASK_OUTPUT_LINES: usize = 8;
const MASK_OUTPUT_SIDE_BYTES: usize = 4 * 1024;

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
        turns: TurnLifecycle::new(),
        processes: Mutex::new(HashMap::new()),
        thread_locks: StdMutex::new(HashMap::new()),
        skill_store,
        skill_generation: Mutex::new(None),
        platform,
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
    turns: TurnLifecycle,
    processes: Mutex<HashMap<ProcessKey, ProcessRecord>>,
    thread_locks: StdMutex<HashMap<i64, Arc<Mutex<()>>>>,
    skill_store: AtraStore,
    skill_generation: Mutex<Option<Arc<skills::SkillGeneration>>>,
    platform: Option<Arc<PlatformStore>>,
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

#[derive(Clone, Hash, PartialEq, Eq)]
struct ProcessKey {
    thread_id: i64,
    runner: String,
    process_id: String,
}

#[derive(Clone)]
struct ProcessRecord {
    handle: String,
    command: String,
    started_at_ms: i64,
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
        let active = self.turns.start(thread_id).await?;
        updates
            .send(ModelStreamEvent::TurnStarted { thread_id })
            .context("turn stream closed before turn started")?;
        let mut cancel_requested = active.cancel_requested();
        let mut cancellation = active.cancellation();
        let mut turn = Box::pin(async {
            match request {
                ControllerRequest::ThreadSend { thread_id, message } => {
                    self.run_turn(thread_id, message, Some(updates)).await
                }
                ControllerRequest::ThreadContinue { thread_id } => {
                    self.continue_thread(thread_id, Some(updates)).await
                }
                _ => unreachable!("non-streaming request dispatched as streaming"),
            }
        });
        let completed = tokio::select! {
            biased;
            changed = cancel_requested.changed() => {
                changed.context("turn cancellation channel closed")?;
                None
            }
            response = &mut turn => Some(response),
        };
        let mut response = if let Some(response) = completed {
            response
        } else {
            drop(turn);
            cancellation
                .changed()
                .await
                .context("turn cancellation channel closed")?;
            match cancellation
                .borrow()
                .clone()
                .expect("cancellation completed")
            {
                Ok(()) => Ok(ControllerResponse::ThreadCancelled),
                Err(message) => Err(anyhow!(message)),
            }
        };
        if active.is_cancelling() && !matches!(response, Ok(ControllerResponse::ThreadCancelled)) {
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
        if !active.is_cancelling() {
            self.turns.finish(thread_id, &active).await;
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
                let events = protocol_events(
                    self.store
                        .events(thread_id)
                        .await
                        .context("failed to load thread events")?,
                );
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
                let events = protocol_events(
                    self.store
                        .checkpoint_events(checkpoint_id)
                        .await
                        .context("failed to load checkpoint events")?,
                );
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
            ControllerRequest::ThreadProcessList { thread_id } => {
                let records = self
                    .processes
                    .lock()
                    .await
                    .iter()
                    .filter(|(key, _)| key.thread_id == thread_id)
                    .map(|(key, record)| (key.clone(), record.clone()))
                    .collect::<Vec<_>>();
                let mut processes = Vec::with_capacity(records.len());
                for (key, record) in records {
                    let status = self.process_status(&key, &record).await;
                    processes.push(BackgroundProcess {
                        runner: key.runner,
                        process_id: key.process_id,
                        command: record.command,
                        started_at_ms: record.started_at_ms,
                        status,
                    });
                }
                processes.sort_by(|left, right| {
                    left.started_at_ms
                        .cmp(&right.started_at_ms)
                        .then_with(|| left.process_id.cmp(&right.process_id))
                });
                Ok(ControllerResponse::ThreadProcessList { processes })
            }
            ControllerRequest::ThreadProcessInspect {
                thread_id,
                runner,
                process_id,
            } => {
                let key = ProcessKey {
                    thread_id,
                    runner,
                    process_id,
                };
                let record = self
                    .processes
                    .lock()
                    .await
                    .get(&key)
                    .cloned()
                    .context("background process is no longer available")?;
                Ok(ControllerResponse::ThreadProcessInspect {
                    process: self.inspect_process(key, record).await,
                })
            }
            ControllerRequest::ThreadProcessStop {
                thread_id,
                runner,
                process_id,
            } => {
                let key = ProcessKey {
                    thread_id,
                    runner,
                    process_id,
                };
                let record = self
                    .processes
                    .lock()
                    .await
                    .get(&key)
                    .cloned()
                    .context("background process is no longer available")?;
                match self
                    .runner(&key.runner)
                    .await?
                    .request_raw(RunnerRequest::StopProcess {
                        process_handle: record.handle,
                    })
                    .await?
                {
                    RunnerResponse::ProcessStopped { .. } => {
                        self.processes.lock().await.remove(&key);
                        Ok(ControllerResponse::ThreadProcessStopped)
                    }
                    RunnerResponse::Error { message } => bail!("{message}"),
                    _ => bail!("runner returned an invalid stop_process response"),
                }
            }
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
                mode,
            } => {
                tracing::debug!(
                    runner,
                    %command,
                    ?mode,
                    "executing command"
                );
                let runner = self.runner(&runner).await?;
                runner
                    .request(RunnerRequest::ExecCommand {
                        command,
                        mode,
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

    async fn inspect_process(
        &self,
        key: ProcessKey,
        record: ProcessRecord,
    ) -> BackgroundProcessDetail {
        let response = match self.runner(&key.runner).await {
            Ok(runner) => runner.client.inspect(record.handle).await,
            Err(error) => Err(error),
        };
        let (status, output_tail, omitted_bytes) = match response {
            Ok(runner_client::ProcessInspection {
                status,
                output_tail,
                omitted_bytes,
            }) => (status, output_tail, omitted_bytes),
            Err(error) => (
                ProcessStatus::Unavailable {
                    message: format!("{error:#}"),
                },
                String::new(),
                0,
            ),
        };
        BackgroundProcessDetail {
            process: BackgroundProcess {
                runner: key.runner,
                process_id: key.process_id,
                command: record.command,
                started_at_ms: record.started_at_ms,
                status,
            },
            output_tail,
            omitted_bytes,
        }
    }

    async fn process_status(&self, key: &ProcessKey, record: &ProcessRecord) -> ProcessStatus {
        let response = match self.runner(&key.runner).await {
            Ok(runner) => runner.client.status(record.handle.clone()).await,
            Err(error) => Err(error),
        };
        match response {
            Ok(process_status) => process_status,
            Err(error) => ProcessStatus::Unavailable {
                message: format!("{error:#}"),
            },
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
        self.sync_runners(thread_id, updates).await?;
        self.store
            .name_thread_if_unnamed(thread_id, message.clone())
            .await
            .context("failed to name thread")?;
        self.sync_workspace_instructions(thread_id).await?;
        self.append_event(
            thread_id,
            ThreadEventData::UserMessage(MessageEvent { content: message }),
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
        self.sync_runners(thread_id, updates).await?;
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load thread history")?;
        let resumable = events.iter().rev().find(|event| {
            matches!(
                event.data,
                ThreadEventData::UserMessage(_)
                    | ThreadEventData::AssistantMessage(_)
                    | ThreadEventData::ToolCall(_)
                    | ThreadEventData::ToolResult(_)
                    | ThreadEventData::Compaction(_)
            )
        });
        match resumable.map(|event| &event.data) {
            Some(
                ThreadEventData::UserMessage(_)
                | ThreadEventData::ToolResult(_)
                | ThreadEventData::Compaction(_),
            ) => {}
            Some(ThreadEventData::AssistantMessage(_)) => bail!("thread turn is already complete"),
            Some(ThreadEventData::ToolCall(_)) => unreachable!(),
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
                    event.data,
                    ThreadEventData::UserMessage(_)
                        | ThreadEventData::AssistantMessage(_)
                        | ThreadEventData::ToolCall(_)
                        | ThreadEventData::ToolResult(_)
                        | ThreadEventData::Compaction(_)
                )
            })
            .filter(|event| matches!(event.data, ThreadEventData::ToolCall(_)))
        else {
            return Ok(());
        };
        self.turns.clear_approvals(thread_id).await;
        let ThreadEventData::ToolCall(call) = &tool_call.data else {
            unreachable!()
        };
        let (name, call_id, custom) = match call {
            ToolCallEvent::Custom { name, call_id, .. } => {
                (name.as_str(), Some(call_id.as_str()), true)
            }
            ToolCallEvent::Function { name, call_id, .. } => {
                (name.as_str(), call_id.as_deref(), false)
            }
        };
        self.save_tool_result(
            thread_id,
            name,
            call_id,
            ToolOutcome::text("tool execution was interrupted before completion".to_owned()),
            custom,
            updates,
        )
        .await
        .context("failed to save interrupted tool result")
    }

    async fn cancel_thread(&self, thread_id: i64) -> Result<ControllerResponse> {
        let Some(active) = self.turns.begin_cancellation(thread_id).await else {
            return Ok(ControllerResponse::ThreadNotActive);
        };
        let stop = active.request_cancellation().await;
        let cleanup = async {
            let _guard = self.thread_lock(thread_id).lock_owned().await;
            self.turns.clear_approvals(thread_id).await;
            self.prepare_thread_for_turn(thread_id, None).await
        }
        .await;
        let result = match (stop, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(stop), Ok(())) => Err(stop),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Err(stop), Err(cleanup)) => {
                Err(stop.context(format!("turn cleanup also failed: {cleanup:#}")))
            }
        };
        self.turns.finish(thread_id, &active).await;
        let outcome = result.map_err(|error| format!("{error:#}"));
        active.complete_cancellation(outcome.clone());
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
        self.turns.ensure_no_pending_approval(thread_id).await
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
            self.sync_runners(thread_id, updates).await?;
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
            let masked_tokens = self
                .mask_old_command_results(thread_id, &mut events, updates)
                .await?;
            let masked_tokens = i64::try_from(masked_tokens).unwrap_or(i64::MAX);
            let active_history_start = events
                .iter()
                .rposition(|event| matches!(event.data, ThreadEventData::Compaction(_)))
                .map_or(0, |index| index + 1);
            let active_tokens = events[active_history_start..]
                .iter()
                .rev()
                .find_map(|event| match &event.data {
                    ThreadEventData::TokenUsage(event) => Some(&event.usage),
                    _ => None,
                })
                .and_then(|usage| usage["total_tokens"].as_i64())
                .map(|tokens| tokens.saturating_sub(masked_tokens));
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
                    ThreadEventData::ModelRequest(ModelRequestEvent {
                        kind: ModelRequestKind::Compaction,
                        started_at_ms: unix_time_ms(),
                        request,
                        context_window,
                        auto_compact_token_limit,
                        compacted: events
                            .iter()
                            .any(|event| matches!(event.data, ThreadEventData::Compaction(_))),
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
                        Some(InstructionEvent {
                            content: workspace_instructions.content,
                            transition: if transition == "initial" {
                                InstructionTransition::Initial
                            } else {
                                InstructionTransition::Removal
                            },
                        })
                    } else {
                        None
                    };
                    self.store
                        .replace_with_compaction(
                            thread_id,
                            CompactionEvent {
                                items: serde_json::to_value(items)
                                    .map_err(|error| anyhow!(error))?,
                            },
                            workspace_event,
                            skill_event(&events),
                            runner_event(&events),
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
                    ThreadEventData::ModelRequest(ModelRequestEvent {
                        kind: ModelRequestKind::Response,
                        started_at_ms: unix_time_ms(),
                        request,
                        context_window,
                        auto_compact_token_limit,
                        compacted: events
                            .iter()
                            .any(|event| matches!(event.data, ThreadEventData::Compaction(_))),
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
                    .append(
                        thread_id,
                        ThreadEventData::Reasoning(ItemEvent {
                            item: serde_json::to_value(item).map_err(|error| anyhow!(error))?,
                        }),
                    )
                    .await
                    .context("failed to save encrypted reasoning")?;
            }
            if let Some(usage) = completion.token_usage {
                self.append_event(
                    thread_id,
                    ThreadEventData::TokenUsage(TokenUsageEvent {
                        request_sequence,
                        usage: serde_json::to_value(usage).map_err(|error| anyhow!(error))?,
                    }),
                    updates,
                )
                .await
                .context("failed to save token usage")?;
            }
            if !completion.rate_limits.is_empty() {
                self.append_event(
                    thread_id,
                    ThreadEventData::RateLimits(RateLimitsEvent {
                        request_sequence,
                        snapshots: serde_json::to_value(completion.rate_limits)
                            .map_err(|error| anyhow!(error))?,
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

    async fn mask_old_command_results(
        &self,
        thread_id: i64,
        events: &mut Vec<storage::Event>,
        stream_updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<usize> {
        let previous_boundary = storage::latest_frozen_boundary(events);
        let active_through = previous_boundary
            .as_ref()
            .map(|boundary| boundary.through_sequence)
            .into_iter()
            .chain(
                events
                    .iter()
                    .rfind(|event| matches!(event.data, ThreadEventData::Compaction(_)))
                    .map(|event| event.sequence),
            )
            .max();
        let active_start = active_through.map_or(0, |sequence| {
            events.partition_point(|event| event.sequence <= sequence)
        });
        if self.provider.context_tokens(&events[active_start..])? <= ACTIVE_CONTEXT_HIGH_TOKENS {
            return Ok(0);
        }
        let context_tokens_before = self.provider.context_tokens(events)?;
        let mut suffix_start = active_start;
        let mut suffix_end = events.len();
        while suffix_start < suffix_end {
            let middle = suffix_start + (suffix_end - suffix_start) / 2;
            if self.provider.context_tokens(&events[middle..])? <= ACTIVE_CONTEXT_LOW_TOKENS {
                suffix_end = middle;
            } else {
                suffix_start = middle + 1;
            }
        }
        let freeze_through_index = suffix_start.saturating_sub(1);

        let request_sequences = events
            .iter()
            .filter(|event| {
                matches!(&event.data, ThreadEventData::ModelRequest(request) if request.kind == ModelRequestKind::Response)
            })
            .map(|event| event.sequence)
            .collect::<Vec<_>>();
        let mut masked_events = Vec::new();
        let mut through_sequence = None;
        for index in active_start..events.len() {
            let event = &events[index];
            let later_requests =
                request_sequences.partition_point(|sequence| *sequence <= event.sequence);
            let ThreadEventData::ToolResult(result) = &event.data else {
                continue;
            };
            if request_sequences.len() - later_requests < MINIMUM_FULL_RESULT_REQUESTS {
                continue;
            }
            through_sequence = Some(event.sequence);
            if let Some(masked_result) = masked_tool_result(result) {
                let mut event = event.clone();
                match &mut event.data {
                    ThreadEventData::ToolResult(ToolResultEvent::Custom {
                        masked_result: field,
                        ..
                    })
                    | ThreadEventData::ToolResult(ToolResultEvent::Function {
                        masked_result: field,
                        ..
                    }) => *field = Some(serde_json::Value::String(masked_result)),
                    _ => unreachable!(),
                }
                masked_events.push((index, event));
            }
            if index >= freeze_through_index {
                break;
            }
        }
        if masked_events.is_empty() {
            return Ok(0);
        }
        let through_sequence = through_sequence.expect("a masked event was passed");
        let mut masked_sequences = previous_boundary
            .map(|boundary| boundary.masked_sequences)
            .unwrap_or_default();
        masked_sequences.extend(masked_events.iter().map(|(_, event)| event.sequence));
        let boundary_data = FrozenBoundaryEvent {
            through_sequence,
            masked_sequences,
        };
        let mut projected_events = events.clone();
        for (index, event) in &masked_events {
            projected_events[*index] = event.clone();
        }
        projected_events.push(storage::Event {
            sequence: events.last().map_or(0, |event| event.sequence + 1),
            data: ThreadEventData::FrozenBoundary(boundary_data.clone()),
        });
        let masked_tokens =
            context_tokens_before.saturating_sub(self.provider.context_tokens(&projected_events)?);
        if masked_tokens == 0 {
            return Ok(0);
        }
        let sequence = self
            .store
            .freeze_event_payloads(
                thread_id,
                masked_events
                    .iter()
                    .map(|(_, event)| (event.sequence, event.data.clone()))
                    .collect(),
                boundary_data.clone(),
            )
            .await
            .context("failed to mask old command results")?;
        for (index, event) in &masked_events {
            events[*index] = event.clone();
        }
        let boundary = storage::Event {
            sequence,
            data: ThreadEventData::FrozenBoundary(boundary_data),
        };
        events.push(boundary.clone());
        if let Some(stream_updates) = stream_updates {
            let _ = stream_updates.send(ModelStreamEvent::ThreadEvent(protocol_event(boundary)));
            for (_, event) in &masked_events {
                let _ = stream_updates
                    .send(ModelStreamEvent::ThreadEvent(protocol_event(event.clone())));
            }
        }
        Ok(masked_tokens)
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
                ThreadEventData::WorkspaceInstructions(InstructionEvent {
                    content,
                    transition: match transition {
                        "initial" => InstructionTransition::Initial,
                        "replacement" => InstructionTransition::Replacement,
                        "removal" => InstructionTransition::Removal,
                        _ => unreachable!(),
                    },
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
            ThreadEventData::Skills(InstructionEvent {
                content: generation.prompt.clone(),
                transition: match transition {
                    "initial" => InstructionTransition::Initial,
                    "replacement" => InstructionTransition::Replacement,
                    "removal" => InstructionTransition::Removal,
                    _ => unreachable!(),
                },
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

    async fn sync_runners(
        &self,
        thread_id: i64,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<()> {
        let runners = self.list_runners().await?;
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load runner state")?;
        let previous = current_runners(&events);
        if previous.as_ref() == Some(&runners) {
            return Ok(());
        }
        self.append_event(
            thread_id,
            ThreadEventData::Runners(RunnersEvent {
                runners: runners.clone(),
                transition: if previous.is_some() {
                    InstructionTransition::Replacement
                } else {
                    InstructionTransition::Initial
                },
            }),
            updates,
        )
        .await
        .context("failed to save runners")?;
        Ok(())
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
                        ThreadEventData::AssistantMessage(MessageEvent {
                            content: content.clone(),
                        }),
                        updates,
                    )
                    .await
                    .context("failed to save assistant message")?;
                    return Ok(Some(ControllerResponse::TurnCompleted { content }));
                }
                ModelResponse::WebSearch { item } => {
                    self.append_event(
                        thread_id,
                        ThreadEventData::WebSearch(ItemEvent {
                            item: serde_json::to_value(item).map_err(|error| anyhow!(error))?,
                        }),
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
                        ThreadEventData::ToolCall(ToolCallEvent::Function {
                            name: name.clone(),
                            arguments: arguments.clone(),
                            call_id: call_id.clone(),
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
                    if name != "runner" {
                        bail!("model requested unsupported custom tool {name}");
                    }
                    self.append_event(
                        thread_id,
                        ThreadEventData::ToolCall(ToolCallEvent::Custom {
                            call_type: CustomToolType::Custom,
                            item_id: item_id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            call_id: call_id.clone(),
                        }),
                        updates,
                    )
                    .await
                    .context("failed to save tool call")?;
                    let mut results = Vec::new();
                    let mut artifacts = Vec::new();
                    for (index, operation) in parse_runner_input(&input)?.into_iter().enumerate() {
                        let operation_index = index + 1;
                        let runner = operation.runner().to_owned();
                        let operation_name = operation.name();
                        let result_label = operation.result_label();
                        let operation_context = OperationContext {
                            call_id: call_id.clone(),
                            index: operation_index,
                            label: result_label.clone(),
                        };
                        let outcome = self
                            .approve_and_execute(
                                thread_id,
                                operation_name,
                                operation.into_arguments(),
                                Some(&operation_context),
                                updates,
                            )
                            .await?;
                        let result = outcome
                            .result
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| outcome.result.to_string());
                        results.push(format!(
                            "Operation {} [{runner}] {result_label}:\n{}",
                            operation_index, result
                        ));
                        let artifact = ToolArtifact::RunnerOperation(RunnerOperationArtifact {
                            operation: operation_index,
                            runner,
                            label: result_label,
                            result: serde_json::Value::String(result),
                            artifacts: outcome.artifacts,
                        });
                        send_operation_update(
                            Some(&operation_context),
                            updates,
                            RunnerOperationUpdate::Completed {
                                artifact: artifact.clone(),
                            },
                        )?;
                        artifacts.push(artifact);
                    }
                    self.save_tool_result(
                        thread_id,
                        &name,
                        Some(&call_id),
                        ToolOutcome {
                            result: serde_json::Value::String(results.join("\n\n")),
                            artifacts,
                        },
                        true,
                        updates,
                    )
                    .await?;
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
        let result = self
            .approve_and_execute(thread_id, &name, arguments, None, updates)
            .await?;
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

    async fn approve_and_execute(
        &self,
        thread_id: i64,
        name: &str,
        arguments: ToolArguments,
        operation: Option<&OperationContext>,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ToolOutcome> {
        let runner = self.runner(arguments.runner()).await?;
        let decision =
            if arguments.requires_approval() && runner.approval().await == ApprovalPolicy::Ask {
                let arguments_json = serde_json::to_value(&arguments)
                    .context("failed to encode approval arguments")?;
                let (approval_id, approval) = self.turns.register_approval(thread_id).await?;
                updates
                    .context("approval requires a streaming turn")?
                    .send(ModelStreamEvent::ApprovalRequired {
                        approval_id,
                        thread_id,
                        tool: name.to_owned(),
                        arguments: arguments_json,
                        operation_index: operation.map(|operation| operation.index),
                        operation_label: operation.map(|operation| operation.label.clone()),
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
                self.turns
                    .get(thread_id)
                    .await
                    .context("thread has no active turn")?,
            )
        } else {
            None
        };
        let _uncancellable = match &active {
            Some(active) => Some(active.lock_uncancellable().await),
            None => None,
        };
        let result = if allowed {
            self.execute(thread_id, arguments, operation, updates)
                .await?
        } else {
            let reason = decision.and_then(|decision| decision.reason);
            let output = match reason {
                Some(reason) => format!("user denied the tool call: {reason}"),
                None => "user denied the tool call".to_owned(),
            };
            ToolOutcome::text(output)
        };
        Ok(result)
    }

    async fn resolve_approval(
        &self,
        approval_id: u64,
        allowed: bool,
        reason: Option<String>,
    ) -> Result<ControllerResponse> {
        self.turns
            .resolve_approval(approval_id, ApprovalDecision { allowed, reason })
            .await?;
        Ok(ControllerResponse::ApprovalResolved)
    }

    async fn execute(
        &self,
        thread_id: i64,
        arguments: ToolArguments,
        operation: Option<&OperationContext>,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ToolOutcome> {
        match arguments {
            ToolArguments::ExecCommand(arguments) => {
                let runner_name = arguments.runner.clone();
                let command = arguments.command.clone();
                let started_at_ms = checkpoint_time_ms();
                let runner = self.runner(&arguments.runner).await?;
                if let ModelCommandMode::Background { process_id } = &arguments.mode
                    && self.processes.lock().await.contains_key(&ProcessKey {
                        thread_id,
                        runner: runner_name.clone(),
                        process_id: process_id.clone(),
                    })
                {
                    return Ok(ToolOutcome::text(format!(
                        "Process ID '{process_id}' is already in use on Runner {runner_name}"
                    )));
                }
                let response = match arguments.mode {
                    ModelCommandMode::Background { process_id } => {
                        let response = runner
                            .request_raw(RunnerRequest::ExecCommand {
                                command: arguments.command,
                                mode: CommandMode::Background,
                                environment: runner.environment.lock().await.clone(),
                            })
                            .await?;
                        match response {
                            RunnerResponse::ProcessStarted { process_handle } => {
                                self.processes.lock().await.insert(
                                    ProcessKey {
                                        thread_id,
                                        runner: runner_name.clone(),
                                        process_id: process_id.clone(),
                                    },
                                    ProcessRecord {
                                        handle: process_handle,
                                        command: command.clone(),
                                        started_at_ms,
                                    },
                                );
                                RunnerResponse::ProcessStarted {
                                    process_handle: process_id,
                                }
                            }
                            response => response,
                        }
                    }
                    mode @ (ModelCommandMode::Foreground { .. }
                    | ModelCommandMode::Timed { .. }) => {
                        let terminate_on_timeout = matches!(&mode, ModelCommandMode::Timed { .. });
                        let timeout_ms = match mode {
                            ModelCommandMode::Foreground { timeout_ms }
                            | ModelCommandMode::Timed { timeout_ms } => timeout_ms,
                            ModelCommandMode::Background { .. } => unreachable!(),
                        };
                        let active = self
                            .turns
                            .get(thread_id)
                            .await
                            .context("thread has no active turn")?;
                        let response = runner
                            .request_raw(RunnerRequest::StartCommand {
                                command: arguments.command,
                                environment: runner.environment.lock().await.clone(),
                            })
                            .await?;
                        let RunnerResponse::ProcessStarted { process_handle } = response else {
                            bail!("runner returned an invalid start command response");
                        };
                        active
                            .set_process(Arc::clone(&runner), process_handle.clone())
                            .await;
                        send_operation_update(
                            operation,
                            updates,
                            RunnerOperationUpdate::CommandStarted,
                        )?;
                        let deadline =
                            Instant::now() + std::time::Duration::from_millis(timeout_ms);
                        let mut collected = None;
                        let response = loop {
                            let timeout_ms = deadline
                                .saturating_duration_since(Instant::now())
                                .as_millis()
                                .min(1000)
                                .try_into()
                                .unwrap_or(1000);
                            match runner
                                .request_raw(RunnerRequest::WaitProcess {
                                    process_handle: process_handle.clone(),
                                    timeout_ms,
                                })
                                .await?
                            {
                                RunnerResponse::ProcessRunning { output, .. } => {
                                    send_operation_update(
                                        operation,
                                        updates,
                                        RunnerOperationUpdate::CommandOutput {
                                            content: output.content.clone(),
                                            omitted_bytes: output.omitted_bytes,
                                        },
                                    )?;
                                    append_command_output(&mut collected, output);
                                    if Instant::now() >= deadline {
                                        break if terminate_on_timeout {
                                            match runner
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
                                            }
                                        } else {
                                            RunnerResponse::ProcessRunning {
                                                process_handle: process_handle.clone(),
                                                output: collected.take().unwrap(),
                                            }
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
                        active.clear_process().await;
                        response
                    }
                };
                let response = match response {
                    response @ RunnerResponse::ProcessStarted { .. } => response,
                    RunnerResponse::ProcessRunning {
                        process_handle,
                        output,
                    } => {
                        let mut processes = self.processes.lock().await;
                        let process_id = loop {
                            let process_id = atra_id::generate().replace(' ', "-");
                            if valid_process_id(&process_id)
                                && !processes.contains_key(&ProcessKey {
                                    thread_id,
                                    runner: runner_name.clone(),
                                    process_id: process_id.clone(),
                                })
                            {
                                break process_id;
                            }
                        };
                        processes.insert(
                            ProcessKey {
                                thread_id,
                                runner: runner_name.clone(),
                                process_id: process_id.clone(),
                            },
                            ProcessRecord {
                                handle: process_handle,
                                command,
                                started_at_ms,
                            },
                        );
                        RunnerResponse::ProcessRunning {
                            process_handle: process_id,
                            output,
                        }
                    }
                    response => response,
                };
                let artifact = command_artifact(&response, &runner_name)?;
                Ok(ToolOutcome::with_artifact(
                    format_exec_response(&runner_name, response)?,
                    ToolArtifact::CommandExecution(artifact),
                ))
            }
            ToolArguments::ApplyPatch(arguments) => {
                let result = self
                    .runner(&arguments.runner)
                    .await?
                    .client
                    .apply_patch(arguments.patch)
                    .await?;
                let output = format_patch_result(&result);
                Ok(ToolOutcome::with_artifact(
                    output,
                    ToolArtifact::PatchOperations(result),
                ))
            }
            ToolArguments::WaitProcess(arguments) => {
                let runner_name = arguments.runner.clone();
                let process_handle = self
                    .processes
                    .lock()
                    .await
                    .get(&ProcessKey {
                        thread_id,
                        runner: runner_name.clone(),
                        process_id: arguments.process_id.clone(),
                    })
                    .map(|process| process.handle.clone());
                let Some(process_handle) = process_handle else {
                    return Ok(ToolOutcome::text(format!(
                        "Process ID '{}' is not running on Runner {runner_name}",
                        arguments.process_id
                    )));
                };
                let runner = self.runner(&arguments.runner).await?;
                send_operation_update(operation, updates, RunnerOperationUpdate::WaitStarted)?;
                let deadline =
                    Instant::now() + std::time::Duration::from_millis(arguments.timeout_ms);
                let mut collected = None;
                let response = loop {
                    let timeout_ms = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(1000)
                        .try_into()
                        .unwrap_or(1000);
                    match runner
                        .request_raw(RunnerRequest::WaitProcess {
                            process_handle: process_handle.clone(),
                            timeout_ms,
                        })
                        .await?
                    {
                        RunnerResponse::ProcessRunning { output, .. } => {
                            send_operation_update(
                                operation,
                                updates,
                                RunnerOperationUpdate::CommandOutput {
                                    content: output.content.clone(),
                                    omitted_bytes: output.omitted_bytes,
                                },
                            )?;
                            append_command_output(&mut collected, output);
                            if Instant::now() >= deadline {
                                break RunnerResponse::ProcessRunning {
                                    process_handle,
                                    output: collected.take().unwrap(),
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
                let response = match response {
                    RunnerResponse::ProcessRunning { output, .. } => {
                        RunnerResponse::ProcessRunning {
                            process_handle: arguments.process_id.clone(),
                            output,
                        }
                    }
                    response @ RunnerResponse::ProcessFinished { .. } => {
                        self.processes.lock().await.remove(&ProcessKey {
                            thread_id,
                            runner: runner_name.clone(),
                            process_id: arguments.process_id.clone(),
                        });
                        response
                    }
                    response => response,
                };
                let artifact = command_artifact(&response, &runner_name)?;
                Ok(ToolOutcome::with_artifact(
                    format_process_response("wait_process", &runner_name, response)?,
                    ToolArtifact::CommandExecution(artifact),
                ))
            }
            ToolArguments::StopProcess(arguments) => {
                let runner_name = arguments.runner.clone();
                let process_handle = self
                    .processes
                    .lock()
                    .await
                    .get(&ProcessKey {
                        thread_id,
                        runner: runner_name.clone(),
                        process_id: arguments.process_id.clone(),
                    })
                    .map(|process| process.handle.clone());
                let Some(process_handle) = process_handle else {
                    return Ok(ToolOutcome::text(format!(
                        "Process ID '{}' is not running on Runner {runner_name}",
                        arguments.process_id
                    )));
                };
                let response = self
                    .runner(&arguments.runner)
                    .await?
                    .request_raw(RunnerRequest::StopProcess { process_handle })
                    .await?;
                let response = match response {
                    RunnerResponse::ProcessStopped { output } => {
                        self.processes.lock().await.remove(&ProcessKey {
                            thread_id,
                            runner: runner_name.clone(),
                            process_id: arguments.process_id,
                        });
                        RunnerResponse::ProcessStopped { output }
                    }
                    response => response,
                };
                let artifact = command_artifact(&response, &runner_name)?;
                Ok(ToolOutcome::with_artifact(
                    format_process_response("stop_process", &runner_name, response)?,
                    ToolArtifact::CommandExecution(artifact),
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
        let data = if custom {
            ThreadEventData::ToolResult(ToolResultEvent::Custom {
                call_type: CustomToolType::Custom,
                name: name.to_owned(),
                call_id: call_id.map(str::to_owned),
                result: outcome.result,
                artifacts: outcome.artifacts,
                masked_result: None,
            })
        } else {
            ThreadEventData::ToolResult(ToolResultEvent::Function {
                call_type: None,
                name: name.to_owned(),
                call_id: call_id.map(str::to_owned),
                result: outcome.result,
                artifacts: outcome.artifacts,
                masked_result: None,
            })
        };
        self.append_event(thread_id, data, updates)
            .await
            .context("failed to save tool result")?;
        Ok(())
    }

    async fn append_event(
        &self,
        thread_id: i64,
        data: ThreadEventData,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> tokio_rusqlite::Result<i64> {
        let sequence = self.store.append(thread_id, data.clone()).await?;
        if let Some(updates) = updates {
            updates
                .send(ModelStreamEvent::ThreadEvent(ThreadEvent {
                    sequence,
                    data,
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
        .find_map(|event| match &event.data {
            ThreadEventData::WorkspaceInstructions(event) => Some(event),
            _ => None,
        })
        .map_or(
            WorkspaceInstructions {
                content: None,
                tracked: false,
            },
            |event| WorkspaceInstructions {
                content: event.content.clone(),
                tracked: true,
            },
        )
}

fn current_skills(events: &[storage::Event]) -> WorkspaceInstructions {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.data {
            ThreadEventData::Skills(event) => Some(event),
            _ => None,
        })
        .map_or(
            WorkspaceInstructions {
                content: None,
                tracked: false,
            },
            |event| WorkspaceInstructions {
                content: event.content.clone(),
                tracked: true,
            },
        )
}

fn current_runners(events: &[storage::Event]) -> Option<Vec<RunnerInfo>> {
    events.iter().rev().find_map(|event| match &event.data {
        ThreadEventData::Runners(event) => Some(event.runners.clone()),
        _ => None,
    })
}

fn skill_event(events: &[storage::Event]) -> Option<InstructionEvent> {
    let skills = current_skills(events);
    skills.tracked.then(|| InstructionEvent {
        transition: if skills.content.is_some() {
            InstructionTransition::Initial
        } else {
            InstructionTransition::Removal
        },
        content: skills.content,
    })
}

fn runner_event(events: &[storage::Event]) -> Option<RunnersEvent> {
    current_runners(events).map(|runners| RunnersEvent {
        runners,
        transition: InstructionTransition::Initial,
    })
}

#[derive(Deserialize, serde::Serialize)]
struct ExecCommandArguments {
    runner: String,
    command: String,
    #[serde(flatten)]
    mode: ModelCommandMode,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum ModelCommandMode {
    Foreground { timeout_ms: u64 },
    Background { process_id: String },
    Timed { timeout_ms: u64 },
}

#[derive(Deserialize, serde::Serialize)]
struct ApplyPatchArguments {
    runner: String,
    patch: String,
}

#[derive(Deserialize, serde::Serialize)]
struct WaitProcessArguments {
    runner: String,
    process_id: String,
    timeout_ms: u64,
}

#[derive(Deserialize, serde::Serialize)]
struct StopProcessArguments {
    runner: String,
    process_id: String,
}

const FOREGROUND_TIMEOUT_MS: u64 = 10_000;

enum RunnerOperation {
    Command(ExecCommandArguments),
    Patch(ApplyPatchArguments),
    Wait(WaitProcessArguments),
    Stop(StopProcessArguments),
}

impl RunnerOperation {
    fn name(&self) -> &'static str {
        match self {
            Self::Command(_) => "exec_command",
            Self::Patch(_) => "apply_patch",
            Self::Wait(_) => "wait_process",
            Self::Stop(_) => "stop_process",
        }
    }

    fn runner(&self) -> &str {
        match self {
            Self::Command(arguments) => &arguments.runner,
            Self::Patch(arguments) => &arguments.runner,
            Self::Wait(arguments) => &arguments.runner,
            Self::Stop(arguments) => &arguments.runner,
        }
    }

    fn result_label(&self) -> String {
        match self {
            Self::Command(arguments) => match &arguments.mode {
                ModelCommandMode::Foreground { .. } => "Command".to_owned(),
                ModelCommandMode::Background { process_id } => {
                    format!("Background Command {process_id}")
                }
                ModelCommandMode::Timed { timeout_ms } => {
                    format!("Timed Command {timeout_ms}")
                }
            },
            Self::Patch(_) => "Patch".to_owned(),
            Self::Wait(arguments) => {
                format!("Wait {} {}", arguments.process_id, arguments.timeout_ms)
            }
            Self::Stop(arguments) => format!("Stop {}", arguments.process_id),
        }
    }

    fn into_arguments(self) -> ToolArguments {
        match self {
            Self::Command(arguments) => ToolArguments::ExecCommand(arguments),
            Self::Patch(arguments) => ToolArguments::ApplyPatch(arguments),
            Self::Wait(arguments) => ToolArguments::WaitProcess(arguments),
            Self::Stop(arguments) => ToolArguments::StopProcess(arguments),
        }
    }
}

fn parse_runner_input(input: &str) -> Result<Vec<RunnerOperation>> {
    let lines = input.lines().collect::<Vec<_>>();
    if lines.first() != Some(&"BEGIN") || lines.last() != Some(&"END") {
        bail!("runner input must start with 'BEGIN' and end with 'END'");
    }
    let lines = &lines[1..lines.len() - 1];
    let mut index = 0;
    let mut runner = None;
    let mut group_operations = 0;
    let mut operations = Vec::new();

    while index < lines.len() {
        if let Some(name) = lines[index].strip_prefix("*** Runner ") {
            if runner.is_some() && group_operations == 0 {
                bail!("runner group must contain at least one operation");
            }
            if name.is_empty() {
                bail!("runner name cannot be empty");
            }
            runner = Some(name.to_owned());
            group_operations = 0;
            index += 1;
            continue;
        }

        let runner = runner
            .as_ref()
            .context("runner input must start with '*** Runner <runner>'")?
            .clone();
        match lines[index] {
            header
                if header == "*** Command"
                    || header.starts_with("*** Background Command ")
                    || header.starts_with("*** Timed Command ") =>
            {
                let mode = match header {
                    "*** Command" => ModelCommandMode::Foreground {
                        timeout_ms: FOREGROUND_TIMEOUT_MS,
                    },
                    header if header.starts_with("*** Background Command ") => {
                        let process_id = &header["*** Background Command ".len()..];
                        if !valid_process_id(process_id) {
                            bail!("invalid background command process ID '{process_id}'");
                        }
                        ModelCommandMode::Background {
                            process_id: process_id.to_owned(),
                        }
                    }
                    header => ModelCommandMode::Timed {
                        timeout_ms: header["*** Timed Command ".len()..]
                            .parse()
                            .context("timed command milliseconds must be an integer")?,
                    },
                };
                index += 1;
                let end = lines[index..]
                    .iter()
                    .position(|line| *line == "*** End")
                    .map(|offset| index + offset)
                    .context("command must end with '*** End'")?;
                if end == index {
                    bail!("command cannot be empty");
                }
                operations.push(RunnerOperation::Command(ExecCommandArguments {
                    runner,
                    command: lines[index..end].join("\n"),
                    mode,
                }));
                group_operations += 1;
                index = end + 1;
            }
            header if header.starts_with("*** Wait ") => {
                let values = &header["*** Wait ".len()..];
                let (process_id, timeout_ms) = values
                    .split_once(' ')
                    .context("wait must include a process ID and timeout")?;
                if !valid_process_id(process_id) {
                    bail!("invalid wait process ID '{process_id}'");
                }
                operations.push(RunnerOperation::Wait(WaitProcessArguments {
                    runner,
                    process_id: process_id.to_owned(),
                    timeout_ms: timeout_ms
                        .parse()
                        .context("wait timeout must be an integer")?,
                }));
                group_operations += 1;
                index += 1;
            }
            header if header.starts_with("*** Stop ") => {
                let process_id = &header["*** Stop ".len()..];
                if !valid_process_id(process_id) {
                    bail!("invalid stop process ID '{process_id}'");
                }
                operations.push(RunnerOperation::Stop(StopProcessArguments {
                    runner,
                    process_id: process_id.to_owned(),
                }));
                group_operations += 1;
                index += 1;
            }
            "*** Patch" => {
                index += 1;
                let end = lines[index..]
                    .iter()
                    .position(|line| *line == "*** End")
                    .map(|offset| index + offset)
                    .context("patch must end with '*** End'")?;
                if end == index {
                    bail!("patch cannot be empty");
                }
                let patch = lines[index..end].join("\n");
                operations.push(RunnerOperation::Patch(ApplyPatchArguments {
                    runner,
                    patch,
                }));
                group_operations += 1;
                index = end + 1;
            }
            line => bail!("expected runner operation, got '{line}'"),
        }
    }

    if runner.is_none() {
        bail!("runner input must contain at least one runner group");
    }
    if group_operations == 0 {
        bail!("runner group must contain at least one operation");
    }
    Ok(operations)
}

fn valid_process_id(process_id: &str) -> bool {
    process_id.len() <= 64
        && process_id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && process_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum ToolArguments {
    ExecCommand(ExecCommandArguments),
    ApplyPatch(ApplyPatchArguments),
    WaitProcess(WaitProcessArguments),
    StopProcess(StopProcessArguments),
}

struct ToolOutcome {
    result: serde_json::Value,
    artifacts: Vec<ToolArtifact>,
}

impl ToolOutcome {
    fn text(result: String) -> Self {
        Self {
            result: serde_json::Value::String(result),
            artifacts: Vec::new(),
        }
    }

    fn with_artifact(result: String, artifact: ToolArtifact) -> Self {
        Self {
            result: serde_json::Value::String(result),
            artifacts: vec![artifact],
        }
    }
}

impl ToolArguments {
    fn runner(&self) -> &str {
        match self {
            Self::ExecCommand(arguments) => &arguments.runner,
            Self::ApplyPatch(arguments) => &arguments.runner,
            Self::WaitProcess(arguments) => &arguments.runner,
            Self::StopProcess(arguments) => &arguments.runner,
        }
    }

    fn requires_approval(&self) -> bool {
        matches!(self, Self::ExecCommand(_) | Self::ApplyPatch(_))
    }
}

struct OperationContext {
    call_id: String,
    index: usize,
    label: String,
}

fn send_operation_update(
    operation: Option<&OperationContext>,
    updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    update: RunnerOperationUpdate,
) -> Result<()> {
    if let Some(operation) = operation {
        updates
            .context("runner operation update requires a streaming turn")?
            .send(ModelStreamEvent::RunnerOperationUpdate {
                call_id: operation.call_id.clone(),
                operation_index: operation.index,
                update,
            })
            .context("turn stream closed during runner operation")?;
    }
    Ok(())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is before the Unix epoch")
        .as_millis()
        .try_into()
        .expect("current time fits in u64 milliseconds")
}

fn checkpoint_time_ms() -> i64 {
    i64::try_from(unix_time_ms()).expect("current time fits in i64 milliseconds")
}

fn protocol_event(event: storage::Event) -> ThreadEvent {
    ThreadEvent {
        sequence: event.sequence,
        data: event.data,
    }
}

fn protocol_events(mut events: Vec<storage::Event>) -> Vec<ThreadEvent> {
    let masked_sequences = storage::latest_frozen_boundary(&events)
        .map(|boundary| {
            boundary
                .masked_sequences
                .into_iter()
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    for event in &mut events {
        if !masked_sequences.contains(&event.sequence)
            && let ThreadEventData::ToolResult(result) = &mut event.data
        {
            match result {
                ToolResultEvent::Custom { masked_result, .. }
                | ToolResultEvent::Function { masked_result, .. } => *masked_result = None,
            }
        }
    }
    events.into_iter().map(protocol_event).collect()
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
        client
            .initialize()
            .await
            .with_context(|| format!("runner {name} failed to initialize"))?;
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

    async fn stop(&self, process_handle: String) -> Result<CommandOutput> {
        self.client.stop(process_handle).await
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
        match client.prepare_tree(manifest.clone()).await? {
            PrepareTreeResult::MissingObjects(digests) => {
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
                    client
                        .upload_object(digest, executable, STANDARD.encode(compressed))
                        .await?;
                }
            }
            PrepareTreeResult::Ready { digest, path } => {
                if digest != expected_digest {
                    bail!("runner returned tree digest {digest}, expected {expected_digest}");
                }
                return Ok(path);
            }
        }
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
        RunnerResponse::ProcessStopped { output } => Ok(ControllerResponse::ProcessStopped {
            output: format_command_output(&output),
        }),
        RunnerResponse::PatchCompleted { .. } => {
            bail!("runner returned an unexpected patch response")
        }
        RunnerResponse::ProcessInspected { .. } => {
            bail!("runner returned an unexpected process inspection response")
        }
        RunnerResponse::ProcessStatus { .. } => {
            bail!("runner returned an unexpected process status response")
        }
        RunnerResponse::Error { message } => bail!("{message}"),
    }
}

fn format_exec_response(runner: &str, response: RunnerResponse) -> Result<String> {
    match response {
        RunnerResponse::ProcessStarted { process_handle } => {
            Ok(format!("Process started with ID {process_handle}"))
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
        | RunnerResponse::ProcessStopped { .. }
        | RunnerResponse::ProcessInspected { .. }
        | RunnerResponse::ProcessStatus { .. }
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

fn masked_tool_result(payload: &ToolResultEvent) -> Option<String> {
    let (original, artifacts, custom) = match payload {
        ToolResultEvent::Custom {
            result, artifacts, ..
        } => (result.as_str()?, artifacts, true),
        ToolResultEvent::Function {
            result, artifacts, ..
        } => (result.as_str()?, artifacts, false),
    };
    if custom {
        let mut operations = Vec::new();
        let mut command_found = false;
        let mut command_masked = false;
        for artifact in artifacts {
            let ToolArtifact::RunnerOperation(data) = artifact else {
                continue;
            };
            let operation_result = data.result.as_str()?;
            let masked = data.artifacts.iter().find_map(|artifact| match artifact {
                ToolArtifact::CommandExecution(command) => masked_command_result(command),
                ToolArtifact::PatchOperations(_) | ToolArtifact::RunnerOperation(_) => None,
            });
            let result = if let Some(masked) = masked {
                command_found = true;
                if model::text_tokens(&masked) < model::text_tokens(operation_result) {
                    command_masked = true;
                    masked
                } else {
                    operation_result.to_owned()
                }
            } else {
                operation_result.to_owned()
            };
            operations.push(format!(
                "Operation {} [{}] {}:\n{result}",
                data.operation, data.runner, data.label
            ));
        }
        if !command_found {
            return None;
        }
        let masked = operations.join("\n\n");
        return (command_masked && model::text_tokens(&masked) < model::text_tokens(original))
            .then_some(masked);
    }

    let command = artifacts.iter().find_map(|artifact| match artifact {
        ToolArtifact::CommandExecution(command) => Some(command),
        ToolArtifact::PatchOperations(_) | ToolArtifact::RunnerOperation(_) => None,
    })?;
    let masked = masked_command_result(command)?;
    (model::text_tokens(&masked) < model::text_tokens(original)).then_some(masked)
}

fn masked_command_result(command: &CommandExecutionArtifact) -> Option<String> {
    let (output, runner, full_output_path, status) = match command {
        CommandExecutionArtifact::Started { .. } => return None,
        CommandExecutionArtifact::Running {
            output,
            runner,
            full_output_path,
        } => (
            output,
            runner,
            full_output_path,
            "Process is still running".to_owned(),
        ),
        CommandExecutionArtifact::Finished {
            output,
            exit_code,
            runner,
            full_output_path,
        } => (
            output,
            runner,
            full_output_path,
            format!(
                "Process exited with code {}",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
        ),
        CommandExecutionArtifact::TimedOut {
            output,
            runner,
            full_output_path,
        } => (
            output,
            runner,
            full_output_path,
            "Process timed out".to_owned(),
        ),
        CommandExecutionArtifact::Stopped {
            output,
            runner,
            full_output_path,
        } => (
            output,
            runner,
            full_output_path,
            "Process stopped".to_owned(),
        ),
    };
    if output.is_empty() {
        return None;
    }
    let lines = output.lines().collect::<Vec<_>>();
    let head = lines
        .iter()
        .take(MASK_OUTPUT_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let head = truncate_mask_head(&head);
    let tail = lines
        .iter()
        .rev()
        .take(MASK_OUTPUT_LINES)
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let tail = truncate_mask_tail(&tail);
    Some(clean_model_output(&format!(
        "{head}\n\n... output masked ...\n\n{tail}\n\n{status}\n\
         Full output: runner \"{runner}\": {}",
        full_output_path.display()
    )))
}

fn truncate_mask_head(output: &str) -> &str {
    &output[..floor_char_boundary(output, output.len().min(MASK_OUTPUT_SIDE_BYTES))]
}

fn truncate_mask_tail(output: &str) -> &str {
    &output[ceil_char_boundary(output, output.len().saturating_sub(MASK_OUTPUT_SIDE_BYTES))..]
}

fn command_artifact(response: &RunnerResponse, runner: &str) -> Result<CommandExecutionArtifact> {
    Ok(match response {
        RunnerResponse::ProcessStarted { .. } => CommandExecutionArtifact::Started {
            runner: runner.to_owned(),
        },
        RunnerResponse::ProcessRunning { output, .. } => CommandExecutionArtifact::Running {
            output: format_command_output(output),
            runner: runner.to_owned(),
            full_output_path: output.full_output_path.clone(),
        },
        RunnerResponse::ProcessFinished { output, exit_code } => {
            CommandExecutionArtifact::Finished {
                output: format_command_output(output),
                exit_code: *exit_code,
                runner: runner.to_owned(),
                full_output_path: output.full_output_path.clone(),
            }
        }
        RunnerResponse::ProcessTimedOut { output } => CommandExecutionArtifact::TimedOut {
            output: format_command_output(output),
            runner: runner.to_owned(),
            full_output_path: output.full_output_path.clone(),
        },
        RunnerResponse::ProcessStopped { output } => CommandExecutionArtifact::Stopped {
            output: format_command_output(output),
            runner: runner.to_owned(),
            full_output_path: output.full_output_path.clone(),
        },
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
    clean_model_output(&format_command_output_with_location(output, &location))
}

fn clean_model_output(output: &str) -> String {
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
