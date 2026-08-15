use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use atra_patch::ApplyPatchResult;
use atra_platform::PlatformStore;
use atra_protocol::{
    ApprovalPolicy, AssistantMessageEvent, AssistantMessagePhase, CommandEnvironment,
    CommandExecutionArtifact, CommandOutput, CommandTimerState, CompactionEvent, CustomToolType,
    EventSequence, FrozenBoundaryEvent, InstructionEvent, InteractionId, ItemEvent, MessageEvent,
    ModelOutputEvent, ModelRequestEvent, ModelRequestKind, ProcessHandle, ProcessId, ProcessStatus,
    RateLimitsEvent, Runner as RunnerInfo, RunnerOperationArtifact, RunnerOperationUpdate,
    RunnersEvent, SpawnedProcess, ThreadEvent, ThreadEventData, ThreadId, TodoItem, TodoStatus,
    TokenUsageEvent, ToolArtifact, ToolCallEvent, ToolResultEvent,
};
use atra_store::{Store as AtraStore, TreeManifest};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{Mutex, mpsc, watch},
    task::JoinSet,
};

mod agent;
mod commands;
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
mod views;

use commands::TurnProjector;
use lifecycle::TurnLifecycle;
use model::{ModelProvider, ModelResponse, ModelStreamEvent};
use runner::{CommandOutcome, Runner, RunnerConfig};
use runner_client::{
    CallbackEvent, PrepareTreeResult, ProcessSubscription, RunnerClient, WaitOutcome,
};
use runner_pool::{ProcessKey, ProcessRecord, RunnerPool};
use storage::Store;
use tools::*;
use views::Views;

const WORKSPACE_INSTRUCTIONS_MAX_BYTES: usize = 32 * 1024;
const ACTIVE_CONTEXT_HIGH_TOKENS: usize = 96_000;
const ACTIVE_CONTEXT_LOW_TOKENS: usize = 48_000;
const MINIMUM_FULL_RESULT_REQUESTS: usize = 3;
const MASK_OUTPUT_LINES: usize = 8;
const MASK_OUTPUT_SIDE_BYTES: usize = 4 * 1024;

pub async fn codex_login(auth_home: &Path) -> Result<()> {
    model::codex_auth::login(auth_home).await
}

pub async fn codex_logout(auth_home: &Path) -> Result<()> {
    model::codex_auth::logout(auth_home).await
}

pub async fn ollama_login(auth_home: &Path, api_key: String) -> Result<()> {
    let provider = model::ollama(auth_home.to_owned());
    provider.login(Some(api_key)).await?;
    Ok(())
}

pub async fn ollama_logout(auth_home: &Path) -> Result<()> {
    let provider = model::ollama(auth_home.to_owned());
    provider.logout().await
}

pub async fn run(
    endpoint: &Path,
    database: &Path,
    provider_auth_home: &Path,
    data_home: &Path,
    platform: Option<PlatformStore>,
) -> Result<()> {
    let workspace = env::current_dir().context("failed to determine controller workspace")?;
    let store = Store::open(database)
        .await
        .with_context(|| format!("failed to open controller database {}", database.display()))?;
    let prompt_cache_namespace = format!(
        "{:x}",
        Sha256::digest(database.as_os_str().as_encoded_bytes())
    );
    let (providers, default_provider): (HashMap<String, Arc<dyn ModelProvider>>, String) =
        match env::var_os("ATRA_FAKE_MODEL_SCRIPT") {
            Some(path) => {
                let provider = model::fake(Path::new(&path))?;
                (
                    HashMap::from([(provider.id().to_owned(), provider)]),
                    model::FAKE_PROVIDER.to_owned(),
                )
            }
            None => {
                let codex: Arc<dyn ModelProvider> =
                    model::codex(provider_auth_home.join(model::CODEX_PROVIDER)).await;
                let ollama: Arc<dyn ModelProvider> =
                    model::ollama(provider_auth_home.join(model::OLLAMA_PROVIDER));
                (
                    HashMap::from([
                        (codex.id().to_owned(), codex),
                        (ollama.id().to_owned(), ollama),
                    ]),
                    model::CODEX_PROVIDER.to_owned(),
                )
            }
        };
    let platform = platform.map(Arc::new);
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
    let controller_threads = store
        .threads()
        .await
        .context("failed to materialize controller threads")?;
    let controller_providers = provider_states(&providers).await;
    let public_state = atra_protocol::ControllerState::new(
        atra_protocol::ControllerLifecycle::Running,
        controller_threads,
        controller_providers,
        Vec::new(),
    );
    let views = Arc::new(Views::new(public_state));
    let (callback_sender, mut callback_events) = mpsc::unbounded_channel();
    let state = Arc::new(State {
        runners: Arc::new(RunnerPool::new(
            platform,
            Arc::downgrade(&views),
            callback_sender,
        )),
        store,
        providers,
        default_provider,
        turns: TurnLifecycle::new(),
        execution_contexts: Arc::new(StdMutex::new(HashMap::new())),
        skill_store,
        skill_generation: Mutex::new(None),
        data_home: data_home.to_owned(),
        prompt_cache_namespace,
        workspace,
        mutation: Arc::new(Mutex::new(())),
        views,
    });

    let (shutdown, mut shutdown_requested) = watch::channel(false);
    let mut callback_tasks = JoinSet::new();
    loop {
        let has_callback_tasks = !callback_tasks.is_empty();
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let state = Arc::clone(&state);
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        if let Err(error) = connection::handle_client(stream, state, &shutdown).await {
                            tracing::warn!(error = %format!("{error:#}"), "client request failed");
                        }
                    });
                }
                Err(error) => tracing::warn!(%error, "failed to accept controller connection"),
            },
            changed = shutdown_requested.changed() => {
                if changed.is_ok() && *shutdown_requested.borrow() {
                    tracing::info!("controller stopping");
                    break;
                }
            }
            event = callback_events.recv() => {
                let Some(event) = event else {
                    break;
                };
                let callback_state = Arc::clone(&state);
                let CallbackEvent {
                    callback_id,
                    execution_context,
                    request,
                    stdin,
                    cancelled,
                } = event;
                callback_tasks.spawn(async move {
                    tokio::select! {
                        _ = runner_client::execute_callback(
                            callback_state,
                            callback_id,
                            execution_context,
                            request,
                            stdin,
                        ) => {}
                        _ = cancelled => {}
                    }
                });
            }
            completed = callback_tasks.join_next(), if has_callback_tasks => {
                let _ = completed;
            }
        }
    }
    callback_tasks.abort_all();
    while callback_tasks.join_next().await.is_some() {}
    Ok(())
}

pub(crate) struct State {
    runners: Arc<RunnerPool>,
    store: Store,
    providers: HashMap<String, Arc<dyn ModelProvider>>,
    default_provider: String,
    turns: TurnLifecycle,
    execution_contexts: Arc<StdMutex<HashMap<String, ThreadId>>>,
    skill_store: AtraStore,
    skill_generation: Mutex<Option<Arc<skills::SkillGeneration>>>,
    data_home: PathBuf,
    prompt_cache_namespace: String,
    workspace: PathBuf,
    mutation: Arc<Mutex<()>>,
    views: Arc<Views>,
}

#[derive(Clone, PartialEq, Eq)]
enum WorkspaceInstructions {
    Untracked,
    Present(String),
    Removed,
}

async fn provider_states(
    providers: &HashMap<String, Arc<dyn ModelProvider>>,
) -> Vec<atra_protocol::ProviderState> {
    let mut ids = providers.keys().cloned().collect::<Vec<_>>();
    ids.sort_unstable();
    let mut states = Vec::with_capacity(ids.len());
    for id in ids {
        states.push(provider_state(&id, &providers[&id]).await);
    }
    states
}

async fn provider_state(
    id: &str,
    provider: &Arc<dyn ModelProvider>,
) -> atra_protocol::ProviderState {
    let (lifecycle, models, rate_limits) = match provider.login_status().await {
        Ok(model::ProviderLoginStatus::LoginRequired) => (
            atra_protocol::ProviderLifecycle::LoginRequired,
            Vec::new(),
            None,
        ),
        Ok(model::ProviderLoginStatus::LoggedIn(account)) => match provider.models().await {
            Ok(models) => (
                atra_protocol::ProviderLifecycle::LoggedIn { account },
                models,
                provider.rate_limits().await.ok(),
            ),
            Err(error) => (
                atra_protocol::ProviderLifecycle::Failed {
                    message: format!("{error:#}"),
                },
                Vec::new(),
                None,
            ),
        },
        Err(error) => (
            atra_protocol::ProviderLifecycle::Failed {
                message: format!("{error:#}"),
            },
            Vec::new(),
            None,
        ),
    };
    atra_protocol::ProviderState::new(id.to_owned(), lifecycle, models, rate_limits)
}

async fn watch_process(
    views: Weak<Views>,
    process: atra_protocol::ProcessLocator,
    mut subscription: ProcessSubscription,
) {
    loop {
        let Some(views) = views.upgrade() else {
            return;
        };
        let state = match subscription.recv().await {
            Ok(state) => state,
            Err(error) => {
                let _ = views
                    .synchronize_process(
                        &process,
                        String::new(),
                        0,
                        atra_protocol::ProcessStatus::Unavailable {
                            message: format!("{error:#}"),
                        },
                    )
                    .await;
                return;
            }
        };
        match views
            .synchronize_process(
                &process,
                state.output_tail,
                state.omitted_bytes,
                state.status,
            )
            .await
        {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                tracing::error!(
                    process_id = %process.process_id(),
                    error = %format!("{error:#}"),
                    "managed process watcher failed"
                );
                return;
            }
        }
    }
}

impl State {
    async fn lock_mutation(&self) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let guard = Arc::clone(&self.mutation).lock_owned().await;
        self.views.ensure_running().await?;
        Ok(guard)
    }

    async fn shutdown(&self) -> Result<()> {
        let _mutation = self.mutation.lock().await;
        self.views.shutdown().await
    }

    pub(crate) async fn materialize_thread(&self, thread_id: ThreadId) -> Result<()> {
        let _mutation = self.lock_mutation().await?;
        self.materialize_thread_locked(thread_id).await
    }

    pub(crate) async fn materialize_thread_locked(&self, thread_id: ThreadId) -> Result<()> {
        if self.views.has_thread(thread_id).await {
            return Ok(());
        }
        let metadata = self
            .store
            .thread(thread_id)
            .await
            .context("failed to load thread metadata")?;
        let events = protocol_events(
            self.store
                .events(thread_id)
                .await
                .context("failed to load thread events")?,
        );
        let checkpoints = self
            .store
            .checkpoints(thread_id)
            .await
            .context("failed to load thread checkpoints")?;
        let processes = self
            .runners
            .list_processes(thread_id)
            .await
            .into_iter()
            .map(|process| {
                atra_protocol::ProcessSummary::new(
                    atra_protocol::ProcessLocator::new(
                        thread_id,
                        process.runner,
                        process.process_id,
                    ),
                    process.command,
                    process.started_at_ms,
                    process.status,
                )
            })
            .collect();
        let state =
            atra_protocol::ThreadState::materialize(metadata, events, checkpoints, processes)
                .context("failed to materialize thread state")?;
        self.views.insert_thread(state).await;
        Ok(())
    }

    pub(crate) async fn materialize_checkpoint(
        &self,
        checkpoint_id: atra_protocol::CheckpointId,
    ) -> Result<()> {
        let _mutation = self.lock_mutation().await?;
        if self.views.has_checkpoint(checkpoint_id).await {
            return Ok(());
        }
        let metadata = self
            .store
            .checkpoint(checkpoint_id)
            .await
            .context("failed to load checkpoint metadata")?;
        let events = protocol_events(
            self.store
                .checkpoint_events(checkpoint_id)
                .await
                .context("failed to load checkpoint events")?,
        );
        let state = atra_protocol::CheckpointState::materialize(metadata, events)
            .context("failed to materialize checkpoint state")?;
        self.views.insert_checkpoint(state).await;
        Ok(())
    }

    pub(crate) async fn materialize_process(
        &self,
        process: &atra_protocol::ProcessLocator,
    ) -> Result<()> {
        if self.views.has_process(process).await {
            return Ok(());
        }
        let key = ProcessKey {
            thread_id: process.thread_id(),
            runner: process.runner().to_owned(),
            process_id: process.process_id().clone(),
        };
        let record = self
            .runners
            .process(&key)
            .await
            .context("managed process does not exist")?;
        let detail = self.runners.inspect_process(key.clone(), record).await;
        let _mutation = self.lock_mutation().await?;
        if self.views.has_process(process).await {
            return Ok(());
        }
        self.store
            .thread(process.thread_id())
            .await
            .context("managed process thread no longer exists")?;
        self.runners
            .process(&key)
            .await
            .context("managed process is no longer available")?;
        let summary = atra_protocol::ProcessSummary::new(
            process.clone(),
            detail.process.command,
            detail.process.started_at_ms,
            detail.process.status,
        );
        self.views
            .insert_process(atra_protocol::ProcessState::new(
                summary,
                detail.output_tail,
                detail.omitted_bytes,
            ))
            .await?;
        Ok(())
    }

    pub(crate) fn provider(&self, id: &str) -> Result<&Arc<dyn ModelProvider>> {
        self.providers
            .get(id)
            .with_context(|| format!("unknown model provider {id}"))
    }

    async fn start_managed_process(
        &self,
        thread_id: ThreadId,
        runner_name: &str,
        runner: &Runner,
        command: String,
    ) -> Result<ProcessId> {
        let started_at_ms = checkpoint_time_ms();
        let process_id = self
            .runners
            .generate_process_id(thread_id, runner_name)
            .await;
        let started = runner
            .start_command(command.clone(), thread_id, &process_id, None)
            .await?;
        let process_handle = started.handle;
        let registered = async {
            let subscription = runner.subscribe(process_handle.clone()).await?;
            self.register_managed_process(
                ProcessKey {
                    thread_id,
                    runner: runner_name.to_owned(),
                    process_id: process_id.clone(),
                },
                ProcessRecord {
                    handle: process_handle.clone(),
                    command,
                    started_at_ms,
                },
                subscription,
            )
            .await
        }
        .await;
        if let Err(error) = registered {
            if let Err(stop_error) = runner.stop(process_handle).await {
                tracing::warn!(
                    process_id = %process_id,
                    error = %format!("{stop_error:#}"),
                    "failed to stop a process after registration failed"
                );
            }
            return Err(error);
        }
        Ok(process_id)
    }

    async fn register_spawned_processes(
        &self,
        thread_id: ThreadId,
        runner: &str,
        spawned_processes: Vec<SpawnedProcess>,
    ) -> Result<()> {
        let runner_instance = self.runners.get(runner).await?;
        for process in spawned_processes {
            let subscription = runner_instance
                .subscribe(process.process_handle.clone())
                .await?;
            self.register_managed_process(
                ProcessKey {
                    thread_id,
                    runner: runner.to_owned(),
                    process_id: process.process_id,
                },
                ProcessRecord {
                    handle: process.process_handle,
                    command: process.command,
                    started_at_ms: checkpoint_time_ms(),
                },
                subscription,
            )
            .await?;
        }
        Ok(())
    }

    async fn register_managed_process(
        &self,
        key: ProcessKey,
        record: ProcessRecord,
        subscription: ProcessSubscription,
    ) -> Result<()> {
        let _mutation = self.lock_mutation().await?;
        self.store
            .thread(key.thread_id)
            .await
            .context("managed process thread no longer exists")?;
        if !self
            .runners
            .insert_process(key.clone(), record.clone())
            .await
        {
            return Ok(());
        }
        let process = atra_protocol::ProcessLocator::new(
            key.thread_id,
            key.runner.clone(),
            key.process_id.clone(),
        );
        let summary = atra_protocol::ProcessSummary::new(
            process.clone(),
            record.command,
            record.started_at_ms,
            ProcessStatus::Running,
        );
        if let Err(error) = self
            .views
            .insert_process(atra_protocol::ProcessState::new(summary, String::new(), 0))
            .await
        {
            self.runners.remove_process(&key).await;
            return Err(error);
        }
        tokio::spawn(watch_process(
            Arc::downgrade(&self.views),
            process,
            subscription,
        ));
        Ok(())
    }

    async fn launch_runner(
        self: &Arc<Self>,
        name: String,
        description: String,
        approval: ApprovalPolicy,
        command: Vec<String>,
    ) -> Result<()> {
        let cached_generation = self.skill_generation.lock().await.clone();
        let generation = match cached_generation {
            Some(generation) => generation,
            None => {
                let generation = self.collect_skill_generation().await?;
                *self.skill_generation.lock().await = Some(Arc::clone(&generation));
                generation
            }
        };
        self.runners
            .launch(
                name,
                description,
                approval,
                command,
                &self.skill_store,
                &generation,
            )
            .await
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

struct SocketGuard<'a>(&'a Path);

impl Drop for SocketGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
