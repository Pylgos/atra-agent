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
    ApprovalId, ApprovalPolicy, CommandEnvironment, CommandExecutionArtifact, CommandMode,
    CommandOutput, CompactionEvent, ControllerResponse, CustomToolType, EventSequence,
    FrozenBoundaryEvent, InstructionEvent, ItemEvent, MessageEvent, ModelRequestEvent,
    ModelRequestKind, ProcessHandle, ProcessId, RateLimitsEvent, Runner as RunnerInfo,
    RunnerOperationArtifact, RunnerOperationUpdate, RunnersEvent, ThreadEvent, ThreadEventData,
    ThreadId, TokenUsageEvent, ToolArtifact, ToolCallEvent, ToolResultEvent, TurnRequest,
    UnaryRequest,
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
mod runner;
mod runner_client;
mod runner_pool;
mod skills;
mod storage;
mod tools;
mod turn;

use lifecycle::{ApprovalDecision, TurnLifecycle};
use model::{DEFAULT_MODEL, ModelResponse, ModelStreamEvent, Provider};
use runner::{CommandOutcome, Runner, RunnerConfig};
use runner_client::{PrepareTreeResult, RunnerClient, WaitOutcome};
use runner_pool::{ProcessKey, ProcessRecord, RunnerPool};
use storage::Store;
use tools::*;

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
        runners: RunnerPool::new(platform),
        store,
        provider,
        turns: TurnLifecycle::new(),
        thread_locks: StdMutex::new(HashMap::new()),
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
    runners: RunnerPool,
    store: Store,
    provider: Provider,
    turns: TurnLifecycle,
    thread_locks: StdMutex<HashMap<ThreadId, Arc<Mutex<()>>>>,
    skill_store: AtraStore,
    skill_generation: Mutex<Option<Arc<skills::SkillGeneration>>>,
    data_home: PathBuf,
    auth_home: PathBuf,
    prompt_cache_namespace: String,
    workspace: PathBuf,
}

#[derive(Clone, PartialEq, Eq)]
enum WorkspaceInstructions {
    Untracked,
    Present(String),
    Removed,
}

impl State {
    async fn codex_login_status(&self) -> Result<ControllerResponse> {
        match self.provider.login_status().await {
            Some(email) => Ok(ControllerResponse::CodexLoggedIn { email }),
            None => Ok(ControllerResponse::CodexLoginRequired),
        }
    }

    async fn handle(&self, request: UnaryRequest) -> Result<ControllerResponse> {
        match request {
            UnaryRequest::Status => Ok(ControllerResponse::Running),
            UnaryRequest::ThreadCreate { display_name } => {
                let thread_id = self
                    .store
                    .create_thread(display_name, DEFAULT_MODEL.to_owned(), "medium".to_owned())
                    .await
                    .context("failed to create thread")?;
                Ok(ControllerResponse::ThreadCreated { thread_id })
            }
            UnaryRequest::ThreadList => {
                let threads = self
                    .store
                    .threads()
                    .await
                    .context("failed to list threads")?;
                Ok(ControllerResponse::ThreadList { threads })
            }
            UnaryRequest::ModelList => Ok(ControllerResponse::ModelList {
                models: self.provider.models().await?,
            }),
            UnaryRequest::ThreadRename {
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
            UnaryRequest::ThreadSetModel {
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
            UnaryRequest::ThreadEvents { thread_id } => {
                let events = protocol_events(
                    self.store
                        .events(thread_id)
                        .await
                        .context("failed to load thread events")?,
                );
                Ok(ControllerResponse::ThreadEvents { events })
            }
            UnaryRequest::ThreadCheckpointCreate { thread_id } => {
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                self.ensure_no_pending_approval(thread_id).await?;
                let checkpoint_id = self
                    .store
                    .create_checkpoint(thread_id, checkpoint_time_ms(), "manual".to_owned())
                    .await
                    .context("failed to create checkpoint")?;
                Ok(ControllerResponse::ThreadCheckpointCreated { checkpoint_id })
            }
            UnaryRequest::ThreadCheckpointList { thread_id } => {
                let checkpoints = self
                    .store
                    .checkpoints(thread_id)
                    .await
                    .context("failed to list checkpoints")?;
                Ok(ControllerResponse::ThreadCheckpointList { checkpoints })
            }
            UnaryRequest::ThreadCheckpointEvents { checkpoint_id } => {
                let events = protocol_events(
                    self.store
                        .checkpoint_events(checkpoint_id)
                        .await
                        .context("failed to load checkpoint events")?,
                );
                Ok(ControllerResponse::ThreadCheckpointEvents { events })
            }
            UnaryRequest::ThreadFork {
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
            UnaryRequest::ThreadReplaceHistory { thread_id, target } => {
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                self.ensure_no_pending_approval(thread_id).await?;
                self.store
                    .replace_history(thread_id, target, checkpoint_time_ms())
                    .await
                    .context("failed to replace thread history")?;
                Ok(ControllerResponse::ThreadHistoryReplaced)
            }
            UnaryRequest::ThreadCancel { thread_id } => self.cancel_thread(thread_id).await,
            UnaryRequest::ThreadProcessList { thread_id } => {
                let processes = self.runners.list_processes(thread_id).await;
                Ok(ControllerResponse::ThreadProcessList { processes })
            }
            UnaryRequest::ThreadProcessInspect {
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
                    .runners
                    .process(&key)
                    .await
                    .context("background process is no longer available")?;
                Ok(ControllerResponse::ThreadProcessInspect {
                    process: self.runners.inspect_process(key, record).await,
                })
            }
            UnaryRequest::CodexLogin => {
                codex_login(&self.auth_home).await?;
                self.provider.reload_auth().await;
                self.codex_login_status().await
            }
            UnaryRequest::CodexLogout => {
                self.provider.logout().await?;
                Ok(ControllerResponse::CodexLoggedOut)
            }
            UnaryRequest::CodexLoginStatus => self.codex_login_status().await,
            UnaryRequest::ApprovalAllow { approval_id } => {
                self.resolve_approval(approval_id, true, None).await
            }
            UnaryRequest::ApprovalDeny {
                approval_id,
                reason,
            } => self.resolve_approval(approval_id, false, reason).await,
            UnaryRequest::RunnerList => Ok(ControllerResponse::RunnerList {
                runners: self.runners.list().await?,
            }),
            UnaryRequest::RunnerLaunch {
                name,
                description,
                approval,
                command,
            } => {
                self.launch_runner(name, description, approval, command)
                    .await
            }
            UnaryRequest::ExecCommand {
                thread_id,
                runner,
                command,
                mode,
            } => {
                self.store
                    .thread_model(thread_id)
                    .await
                    .context("thread does not exist")?;
                tracing::debug!(
                    runner,
                    %command,
                    ?mode,
                    "executing command"
                );
                let active_runner = self.runners.get(&runner).await?;
                self.execute_controller_command(thread_id, &runner, &active_runner, command, mode)
                    .await
            }
            UnaryRequest::WaitProcess {
                thread_id,
                runner,
                process_id,
                timeout_ms,
            } => {
                let key = ProcessKey {
                    thread_id,
                    runner: runner.clone(),
                    process_id: process_id.clone(),
                };
                let record = self
                    .runners
                    .process(&key)
                    .await
                    .context("background process is no longer available")?;
                match self
                    .runners
                    .get(&runner)
                    .await?
                    .wait(record.handle, timeout_ms)
                    .await?
                {
                    WaitOutcome::Running { output, .. } => Ok(ControllerResponse::ProcessRunning {
                        process_id,
                        output: format_command_output(&output),
                    }),
                    WaitOutcome::Finished { output, exit_code } => {
                        self.runners.remove_process(&key).await;
                        Ok(ControllerResponse::ProcessFinished {
                            output: format_command_output(&output),
                            exit_code,
                        })
                    }
                }
            }
            UnaryRequest::StopProcess {
                thread_id,
                runner,
                process_id,
            } => {
                let output = self
                    .runners
                    .stop_process(&ProcessKey {
                        thread_id,
                        runner,
                        process_id,
                    })
                    .await?;
                Ok(ControllerResponse::ProcessStopped {
                    output: format_command_output(&output),
                })
            }
        }
    }

    async fn execute_controller_command(
        &self,
        thread_id: ThreadId,
        runner_name: &str,
        runner: &Runner,
        command: String,
        mode: CommandMode,
    ) -> Result<ControllerResponse> {
        let started_at_ms = checkpoint_time_ms();
        let process_handle = runner.start_command(command.clone()).await?;
        if mode == CommandMode::Background {
            let process_id = self
                .runners
                .register_generated_process(
                    thread_id,
                    runner_name,
                    process_handle,
                    command,
                    started_at_ms,
                )
                .await;
            return Ok(ControllerResponse::ProcessStarted { process_id });
        }

        let deadline = match mode {
            CommandMode::Foreground { timeout_ms } => timeout_ms
                .map(|timeout_ms| Instant::now() + std::time::Duration::from_millis(timeout_ms)),
            CommandMode::Background => unreachable!(),
        };
        let mut collected = None;
        loop {
            let timeout_ms = deadline.map_or(1000, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(1000)
                    .try_into()
                    .unwrap_or(1000)
            });
            match runner.wait(process_handle.clone(), timeout_ms).await? {
                WaitOutcome::Running { output, .. } => {
                    append_command_output(&mut collected, output);
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        let process_id = self
                            .runners
                            .register_generated_process(
                                thread_id,
                                runner_name,
                                process_handle,
                                command,
                                started_at_ms,
                            )
                            .await;
                        return Ok(ControllerResponse::ProcessRunning {
                            process_id,
                            output: format_command_output(&collected.take().unwrap()),
                        });
                    }
                }
                WaitOutcome::Finished { output, exit_code } => {
                    append_command_output(&mut collected, output);
                    let output = collected.take().unwrap();
                    tracing::info!(
                        ?exit_code,
                        output_bytes = output.content.len(),
                        "process finished"
                    );
                    return Ok(ControllerResponse::ProcessFinished {
                        output: format_command_output(&output),
                        exit_code,
                    });
                }
            }
        }
    }

    async fn launch_runner(
        &self,
        name: String,
        description: String,
        approval: ApprovalPolicy,
        command: Vec<String>,
    ) -> Result<ControllerResponse> {
        let cached_generation = self.skill_generation.lock().await.clone();
        let generation = match cached_generation {
            Some(generation) => generation,
            None => {
                let generation = self.collect_skill_generation().await?;
                *self.skill_generation.lock().await = Some(Arc::clone(&generation));
                generation
            }
        };
        if self
            .runners
            .launch(
                name,
                description,
                approval,
                command,
                &self.skill_store,
                &generation,
            )
            .await?
        {
            Ok(ControllerResponse::Launched)
        } else {
            Ok(ControllerResponse::AlreadyRunning)
        }
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
        .map_or(WorkspaceInstructions::Untracked, |event| match event {
            InstructionEvent::Initial(content) | InstructionEvent::Replacement(content) => {
                WorkspaceInstructions::Present(content.clone())
            }
            InstructionEvent::Removal => WorkspaceInstructions::Removed,
        })
}

fn current_skills(events: &[storage::Event]) -> WorkspaceInstructions {
    events
        .iter()
        .rev()
        .find_map(|event| match &event.data {
            ThreadEventData::Skills(event) => Some(event),
            _ => None,
        })
        .map_or(WorkspaceInstructions::Untracked, |event| match event {
            InstructionEvent::Initial(content) | InstructionEvent::Replacement(content) => {
                WorkspaceInstructions::Present(content.clone())
            }
            InstructionEvent::Removal => WorkspaceInstructions::Removed,
        })
}

fn current_runners(events: &[storage::Event]) -> Option<Vec<RunnerInfo>> {
    events.iter().rev().find_map(|event| match &event.data {
        ThreadEventData::Runners(RunnersEvent::Initial(runners))
        | ThreadEventData::Runners(RunnersEvent::Replacement(runners)) => Some(runners.clone()),
        _ => None,
    })
}

fn skill_event(events: &[storage::Event]) -> Option<InstructionEvent> {
    let skills = current_skills(events);
    match skills {
        WorkspaceInstructions::Untracked => None,
        WorkspaceInstructions::Present(content) => Some(InstructionEvent::Initial(content)),
        WorkspaceInstructions::Removed => Some(InstructionEvent::Removal),
    }
}

fn runner_event(events: &[storage::Event]) -> Option<RunnersEvent> {
    current_runners(events).map(RunnersEvent::Initial)
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
