use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ApprovalPolicy, CheckpointId, EventSequence, HistoryTarget, InteractionId, Model, ProcessId,
    ProcessStatus, Runner, RunnerOperationUpdate, Thread, ThreadCheckpoint, ThreadEvent, ThreadId,
};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerLifecycle {
    Running,
    Stopping,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderLifecycle {
    LoggedOut,
    LoggingIn,
    LoggingOut,
    LoginRequired,
    LoggedIn { account: Option<String> },
    Refreshing,
    Failed { message: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderState {
    id: String,
    lifecycle: ProviderLifecycle,
    models: Vec<Model>,
    rate_limits: Option<Value>,
}

impl ProviderState {
    pub fn new(
        id: String,
        lifecycle: ProviderLifecycle,
        models: Vec<Model>,
        rate_limits: Option<Value>,
    ) -> Self {
        Self {
            id,
            lifecycle,
            models,
            rate_limits,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn lifecycle(&self) -> &ProviderLifecycle {
        &self.lifecycle
    }

    pub fn models(&self) -> &[Model] {
        &self.models
    }

    pub fn rate_limits(&self) -> Option<&Value> {
        self.rate_limits.as_ref()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerLifecycle {
    Launching,
    Running,
    Failed { message: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerState {
    runner: Runner,
    lifecycle: RunnerLifecycle,
}

impl RunnerState {
    pub fn new(runner: Runner, lifecycle: RunnerLifecycle) -> Self {
        Self { runner, lifecycle }
    }

    pub fn runner(&self) -> &Runner {
        &self.runner
    }

    pub fn lifecycle(&self) -> &RunnerLifecycle {
        &self.lifecycle
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerState {
    lifecycle: ControllerLifecycle,
    threads: Vec<Thread>,
    thread_statuses: Vec<ThreadStatus>,
    providers: Vec<ProviderState>,
    runners: Vec<RunnerState>,
}

impl ControllerState {
    pub fn new(
        lifecycle: ControllerLifecycle,
        threads: Vec<Thread>,
        providers: Vec<ProviderState>,
        runners: Vec<RunnerState>,
    ) -> Self {
        let thread_statuses = threads
            .iter()
            .map(|thread| ThreadStatus {
                thread_id: thread.id,
                status: AgentStatus::Idle,
            })
            .collect();
        Self {
            lifecycle,
            threads,
            thread_statuses,
            providers,
            runners,
        }
    }

    pub fn lifecycle(&self) -> ControllerLifecycle {
        self.lifecycle
    }

    pub fn threads(&self) -> &[Thread] {
        &self.threads
    }

    pub fn thread_status(&self, thread_id: ThreadId) -> Option<AgentStatus> {
        self.thread_statuses
            .iter()
            .find(|status| status.thread_id == thread_id)
            .map(|status| status.status)
    }

    pub fn providers(&self) -> &[ProviderState] {
        &self.providers
    }

    pub fn runners(&self) -> &[RunnerState] {
        &self.runners
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControllerOperation {
    LifecycleChanged {
        lifecycle: ControllerLifecycle,
    },
    ThreadAdded {
        thread: Thread,
    },
    ThreadUpdated {
        thread: Thread,
    },
    ThreadRemoved {
        thread_id: ThreadId,
    },
    ThreadStatusUpdated {
        thread_id: ThreadId,
        status: AgentStatus,
    },
    ProviderUpdated {
        provider: ProviderState,
    },
    RunnerUpdated {
        runner: RunnerState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControllerChange {
    Lifecycle,
    Thread(ThreadId),
    Provider(String),
    Runner(String),
}

impl ControllerOperation {
    pub fn apply(self, state: &mut ControllerState) -> Result<ControllerChange, ApplyError> {
        match self {
            Self::LifecycleChanged { lifecycle } => {
                state.lifecycle = lifecycle;
                Ok(ControllerChange::Lifecycle)
            }
            Self::ThreadAdded { thread } => {
                if state.threads.iter().any(|current| current.id == thread.id) {
                    return Err(ApplyError::new("thread already exists"));
                }
                let id = thread.id;
                state.threads.insert(0, thread);
                state.thread_statuses.push(ThreadStatus {
                    thread_id: id,
                    status: AgentStatus::Idle,
                });
                Ok(ControllerChange::Thread(id))
            }
            Self::ThreadUpdated { thread } => {
                let id = thread.id;
                let current = state
                    .threads
                    .iter_mut()
                    .find(|current| current.id == id)
                    .ok_or_else(|| ApplyError::new("thread does not exist"))?;
                *current = thread;
                Ok(ControllerChange::Thread(id))
            }
            Self::ThreadRemoved { thread_id } => {
                let index = state
                    .threads
                    .iter()
                    .position(|thread| thread.id == thread_id)
                    .ok_or_else(|| ApplyError::new("thread does not exist"))?;
                state.threads.remove(index);
                state
                    .thread_statuses
                    .retain(|status| status.thread_id != thread_id);
                Ok(ControllerChange::Thread(thread_id))
            }
            Self::ThreadStatusUpdated { thread_id, status } => {
                let current = state
                    .thread_statuses
                    .iter_mut()
                    .find(|current| current.thread_id == thread_id)
                    .ok_or_else(|| ApplyError::new("thread does not exist"))?;
                current.status = status;
                Ok(ControllerChange::Thread(thread_id))
            }
            Self::ProviderUpdated { provider } => {
                let id = provider.id.clone();
                upsert_by(&mut state.providers, provider, |provider| &provider.id);
                Ok(ControllerChange::Provider(id))
            }
            Self::RunnerUpdated { runner } => {
                let name = runner.runner.name.clone();
                upsert_by(&mut state.runners, runner, |runner| &runner.runner.name);
                Ok(ControllerChange::Runner(name))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Running,
    Compacting,
    AwaitingQuestion,
    AwaitingApproval,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct ThreadStatus {
    thread_id: ThreadId,
    status: AgentStatus,
}

fn upsert_by<T, K: PartialEq>(values: &mut Vec<T>, value: T, key: impl Fn(&T) -> &K) {
    if let Some(index) = values
        .iter()
        .position(|current| key(current) == key(&value))
    {
        values[index] = value;
    } else {
        values.push(value);
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ActiveItemId(pub u64);

impl fmt::Display for ActiveItemId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Running,
    Retrying,
    AwaitingInput,
    Cancelling,
    Compacting,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryStatus {
    summary: String,
    current: u64,
    max: u64,
}

impl RetryStatus {
    pub fn new(summary: String, current: u64, max: u64) -> Self {
        Self {
            summary,
            current,
            max,
        }
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn current(&self) -> u64 {
        self.current
    }

    pub fn max(&self) -> u64 {
        self.max
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActiveItemData {
    Assistant {
        content: String,
    },
    Reasoning {
        content: String,
    },
    WebSearch {
        item_id: String,
        action: Option<Value>,
    },
    ToolCall {
        item_id: String,
        name: String,
        input: String,
    },
    RunnerTool {
        call_id: String,
        operation_index: usize,
        update: RunnerOperationUpdate,
    },
}

impl ActiveItemData {
    fn append_text(&mut self, content: &str) -> Result<(), ApplyError> {
        match self {
            Self::Assistant { content: current }
            | Self::Reasoning { content: current }
            | Self::ToolCall { input: current, .. } => {
                current.push_str(content);
                Ok(())
            }
            _ => Err(ApplyError::new(
                "active item does not contain appendable text",
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveItem {
    id: ActiveItemId,
    data: ActiveItemData,
}

impl ActiveItem {
    pub fn new(id: ActiveItemId, data: ActiveItemData) -> Self {
        Self { id, data }
    }

    pub fn id(&self) -> ActiveItemId {
        self.id
    }

    pub fn data(&self) -> &ActiveItemData {
        &self.data
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingApproval {
    id: InteractionId,
    tool: String,
    arguments: Value,
    operation_index: Option<usize>,
    operation_label: Option<String>,
}

impl PendingApproval {
    pub fn new(
        id: InteractionId,
        tool: String,
        arguments: Value,
        operation_index: Option<usize>,
        operation_label: Option<String>,
    ) -> Self {
        Self {
            id,
            tool,
            arguments,
            operation_index,
            operation_label,
        }
    }

    pub fn id(&self) -> InteractionId {
        self.id
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn arguments(&self) -> &Value {
        &self.arguments
    }

    pub fn operation_index(&self) -> Option<usize> {
        self.operation_index
    }

    pub fn operation_label(&self) -> Option<&str> {
        self.operation_label.as_deref()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub recommended_options: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingQuestionRequest {
    pub id: InteractionId,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PendingInteraction {
    Approval(PendingApproval),
    Questions(PendingQuestionRequest),
}

impl PendingInteraction {
    pub fn id(&self) -> InteractionId {
        match self {
            Self::Approval(approval) => approval.id(),
            Self::Questions(request) => request.id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionAnswer {
    pub selected_option: Option<String>,
    pub note: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveTurn {
    phase: TurnPhase,
    items: Vec<ActiveItem>,
    pending_interaction: Option<PendingInteraction>,
    retry: Option<Box<RetryStatus>>,
}

impl ActiveTurn {
    pub fn new(phase: TurnPhase) -> Self {
        Self {
            phase,
            items: Vec::new(),
            pending_interaction: None,
            retry: None,
        }
    }

    pub fn phase(&self) -> TurnPhase {
        self.phase
    }

    pub fn items(&self) -> &[ActiveItem] {
        &self.items
    }

    pub fn pending_approval(&self) -> Option<&PendingApproval> {
        match self.pending_interaction.as_ref() {
            Some(PendingInteraction::Approval(approval)) => Some(approval),
            _ => None,
        }
    }

    pub fn pending_question(&self) -> Option<&PendingQuestionRequest> {
        match self.pending_interaction.as_ref() {
            Some(PendingInteraction::Questions(request)) => Some(request),
            _ => None,
        }
    }

    pub fn pending_interaction(&self) -> Option<&PendingInteraction> {
        self.pending_interaction.as_ref()
    }

    pub fn retry(&self) -> Option<&RetryStatus> {
        self.retry.as_deref()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed { message: String },
}

#[derive(Clone, Debug, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessLocator {
    thread_id: ThreadId,
    runner: String,
    process_id: ProcessId,
}

impl ProcessLocator {
    pub fn new(thread_id: ThreadId, runner: String, process_id: ProcessId) -> Self {
        Self {
            thread_id,
            runner,
            process_id,
        }
    }

    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    pub fn runner(&self) -> &str {
        &self.runner
    }

    pub fn process_id(&self) -> &ProcessId {
        &self.process_id
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSummary {
    locator: ProcessLocator,
    command: String,
    started_at_ms: i64,
    status: ProcessStatus,
}

impl ProcessSummary {
    pub fn new(
        locator: ProcessLocator,
        command: String,
        started_at_ms: i64,
        status: ProcessStatus,
    ) -> Self {
        Self {
            locator,
            command,
            started_at_ms,
            status,
        }
    }

    pub fn locator(&self) -> &ProcessLocator {
        &self.locator
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }

    pub fn status(&self) -> &ProcessStatus {
        &self.status
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadState {
    metadata: Thread,
    events: Vec<ThreadEvent>,
    active_turn: Option<ActiveTurn>,
    last_outcome: Option<TurnOutcome>,
    checkpoints: Vec<ThreadCheckpoint>,
    processes: Vec<ProcessSummary>,
}

impl ThreadState {
    pub fn materialize(
        metadata: Thread,
        events: Vec<ThreadEvent>,
        checkpoints: Vec<ThreadCheckpoint>,
        processes: Vec<ProcessSummary>,
    ) -> Result<Self, ApplyError> {
        validate_events(&events)?;
        if checkpoints
            .iter()
            .any(|checkpoint| checkpoint.thread_id != metadata.id)
        {
            return Err(ApplyError::new("checkpoint belongs to another thread"));
        }
        if processes
            .iter()
            .any(|process| process.locator.thread_id != metadata.id)
        {
            return Err(ApplyError::new("process belongs to another thread"));
        }
        Ok(Self {
            metadata,
            events,
            active_turn: None,
            last_outcome: None,
            checkpoints,
            processes,
        })
    }

    pub fn metadata(&self) -> &Thread {
        &self.metadata
    }

    pub fn events(&self) -> &[ThreadEvent] {
        &self.events
    }

    pub fn active_turn(&self) -> Option<&ActiveTurn> {
        self.active_turn.as_ref()
    }

    pub fn last_outcome(&self) -> Option<&TurnOutcome> {
        self.last_outcome.as_ref()
    }

    pub fn checkpoints(&self) -> &[ThreadCheckpoint] {
        &self.checkpoints
    }

    pub fn processes(&self) -> &[ProcessSummary] {
        &self.processes
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadOperation {
    MetadataUpdated {
        metadata: Thread,
    },
    EventAppended {
        event: ThreadEvent,
    },
    ActiveTurnStarted {
        phase: TurnPhase,
    },
    ActiveItemAdded {
        item: ActiveItem,
    },
    ActiveTextAppended {
        id: ActiveItemId,
        content: String,
    },
    ActiveWebSearchUpdated {
        id: ActiveItemId,
        action: Option<Value>,
    },
    ActiveRunnerUpdated {
        id: ActiveItemId,
        update: RunnerOperationUpdate,
    },
    ActiveItemDiscarded {
        id: ActiveItemId,
    },
    ActiveItemFinalized {
        active_id: ActiveItemId,
        event: ThreadEvent,
    },
    PhaseChanged {
        phase: TurnPhase,
    },
    RetryScheduled {
        retry: RetryStatus,
    },
    InteractionRequested {
        interaction: PendingInteraction,
    },
    InteractionResolved {
        interaction_id: InteractionId,
    },
    TurnFinished {
        outcome: TurnOutcome,
    },
    EventsReplaced {
        events: Vec<ThreadEvent>,
    },
    CheckpointAdded {
        checkpoint: ThreadCheckpoint,
    },
    ProcessUpdated {
        process: ProcessSummary,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadChange {
    Metadata,
    Event(EventSequence),
    ActiveItem(ActiveItemId),
    ActiveItemFinalized {
        active_id: ActiveItemId,
        sequence: EventSequence,
    },
    Phase,
    Interaction,
    TurnFinished,
    HistoryReplaced,
    Checkpoint(CheckpointId),
    Process(ProcessLocator),
}

impl ThreadOperation {
    pub fn apply(self, state: &mut ThreadState) -> Result<ThreadChange, ApplyError> {
        match self {
            Self::MetadataUpdated { metadata } => {
                if metadata.id != state.metadata.id {
                    return Err(ApplyError::new("thread metadata id changed"));
                }
                state.metadata = metadata;
                Ok(ThreadChange::Metadata)
            }
            Self::EventAppended { event } => append_event(&mut state.events, event),
            Self::ActiveTurnStarted { phase } => {
                if state.active_turn.is_some() {
                    return Err(ApplyError::new("thread already has an active turn"));
                }
                state.active_turn = Some(ActiveTurn::new(phase));
                state.last_outcome = None;
                Ok(ThreadChange::Phase)
            }
            Self::ActiveItemAdded { item } => {
                let turn = active_turn_mut(state)?;
                if turn.items.iter().any(|current| current.id == item.id) {
                    return Err(ApplyError::new("active item id already exists"));
                }
                let id = item.id;
                turn.items.push(item);
                Ok(ThreadChange::ActiveItem(id))
            }
            Self::ActiveTextAppended { id, content } => {
                let item = active_turn_mut(state)?
                    .items
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or_else(|| ApplyError::new("active item does not exist"))?;
                item.data.append_text(&content)?;
                Ok(ThreadChange::ActiveItem(id))
            }
            Self::ActiveWebSearchUpdated { id, action } => {
                let item = active_turn_mut(state)?
                    .items
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or_else(|| ApplyError::new("active item does not exist"))?;
                let ActiveItemData::WebSearch {
                    action: current, ..
                } = &mut item.data
                else {
                    return Err(ApplyError::new("active item is not a web search"));
                };
                *current = action;
                Ok(ThreadChange::ActiveItem(id))
            }
            Self::ActiveRunnerUpdated { id, update } => {
                let item = active_turn_mut(state)?
                    .items
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or_else(|| ApplyError::new("active item does not exist"))?;
                let ActiveItemData::RunnerTool {
                    update: current, ..
                } = &mut item.data
                else {
                    return Err(ApplyError::new("active item is not a Runner tool"));
                };
                *current = update;
                Ok(ThreadChange::ActiveItem(id))
            }
            Self::ActiveItemDiscarded { id } => {
                let turn = active_turn_mut(state)?;
                let index = turn
                    .items
                    .iter()
                    .position(|item| item.id == id)
                    .ok_or_else(|| ApplyError::new("active item does not exist"))?;
                turn.items.remove(index);
                Ok(ThreadChange::ActiveItem(id))
            }
            Self::ActiveItemFinalized { active_id, event } => {
                let index = state
                    .active_turn
                    .as_ref()
                    .ok_or_else(|| ApplyError::new("thread has no active turn"))?
                    .items
                    .iter()
                    .position(|item| item.id == active_id)
                    .ok_or_else(|| ApplyError::new("active item does not exist"))?;
                validate_appended_event(&state.events, &event)?;
                let sequence = event.sequence;
                state.events.push(event);
                state
                    .active_turn
                    .as_mut()
                    .expect("active turn was validated")
                    .items
                    .remove(index);
                Ok(ThreadChange::ActiveItemFinalized {
                    active_id,
                    sequence,
                })
            }
            Self::PhaseChanged { phase } => {
                let turn = active_turn_mut(state)?;
                turn.phase = phase;
                turn.retry = None;
                Ok(ThreadChange::Phase)
            }
            Self::RetryScheduled { retry } => {
                let turn = active_turn_mut(state)?;
                turn.phase = TurnPhase::Retrying;
                turn.retry = Some(Box::new(retry));
                Ok(ThreadChange::Phase)
            }
            Self::InteractionRequested { interaction } => {
                let turn = active_turn_mut(state)?;
                if turn.pending_interaction.is_some() {
                    return Err(ApplyError::new("thread already has a pending interaction"));
                }
                turn.phase = TurnPhase::AwaitingInput;
                turn.retry = None;
                turn.pending_interaction = Some(interaction);
                Ok(ThreadChange::Interaction)
            }
            Self::InteractionResolved { interaction_id } => {
                let turn = active_turn_mut(state)?;
                if turn.phase != TurnPhase::AwaitingInput {
                    return Err(ApplyError::new("thread is not awaiting input"));
                }
                let pending = turn
                    .pending_interaction
                    .as_ref()
                    .ok_or_else(|| ApplyError::new("thread has no pending interaction"))?;
                if pending.id() != interaction_id {
                    return Err(ApplyError::new("interaction id does not match"));
                }
                turn.pending_interaction = None;
                turn.phase = TurnPhase::Running;
                turn.retry = None;
                Ok(ThreadChange::Interaction)
            }
            Self::TurnFinished { outcome } => {
                if state.active_turn.take().is_none() {
                    return Err(ApplyError::new("thread has no active turn"));
                }
                state.last_outcome = Some(outcome);
                Ok(ThreadChange::TurnFinished)
            }
            Self::EventsReplaced { events } => {
                validate_events(&events)?;
                state.events = events;
                Ok(ThreadChange::HistoryReplaced)
            }
            Self::CheckpointAdded { checkpoint } => {
                if checkpoint.thread_id != state.metadata.id {
                    return Err(ApplyError::new("checkpoint belongs to another thread"));
                }
                if state
                    .checkpoints
                    .iter()
                    .any(|current| current.id == checkpoint.id)
                {
                    return Err(ApplyError::new("checkpoint already exists"));
                }
                let id = checkpoint.id;
                state.checkpoints.insert(0, checkpoint);
                Ok(ThreadChange::Checkpoint(id))
            }
            Self::ProcessUpdated { process } => {
                if process.locator.thread_id != state.metadata.id {
                    return Err(ApplyError::new("process belongs to another thread"));
                }
                let locator = process.locator.clone();
                upsert_by(&mut state.processes, process, |process| &process.locator);
                Ok(ThreadChange::Process(locator))
            }
        }
    }
}

fn active_turn_mut(state: &mut ThreadState) -> Result<&mut ActiveTurn, ApplyError> {
    state
        .active_turn
        .as_mut()
        .ok_or_else(|| ApplyError::new("thread has no active turn"))
}

fn append_event(
    events: &mut Vec<ThreadEvent>,
    event: ThreadEvent,
) -> Result<ThreadChange, ApplyError> {
    validate_appended_event(events, &event)?;
    let sequence = event.sequence;
    events.push(event);
    Ok(ThreadChange::Event(sequence))
}

fn validate_appended_event(events: &[ThreadEvent], event: &ThreadEvent) -> Result<(), ApplyError> {
    if events
        .last()
        .is_some_and(|current| current.sequence >= event.sequence)
    {
        return Err(ApplyError::new("event sequence is not append-only"));
    }
    Ok(())
}

fn validate_events(events: &[ThreadEvent]) -> Result<(), ApplyError> {
    if events
        .windows(2)
        .any(|pair| pair[0].sequence >= pair[1].sequence)
    {
        return Err(ApplyError::new("events are not in strict sequence order"));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointState {
    metadata: ThreadCheckpoint,
    events: Vec<ThreadEvent>,
}

impl CheckpointState {
    pub fn materialize(
        metadata: ThreadCheckpoint,
        events: Vec<ThreadEvent>,
    ) -> Result<Self, ApplyError> {
        validate_events(&events)?;
        Ok(Self { metadata, events })
    }

    pub fn metadata(&self) -> &ThreadCheckpoint {
        &self.metadata
    }

    pub fn events(&self) -> &[ThreadEvent] {
        &self.events
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessState {
    process: ProcessSummary,
    output_tail: String,
    omitted_bytes: usize,
}

impl ProcessState {
    pub fn new(process: ProcessSummary, output_tail: String, omitted_bytes: usize) -> Self {
        Self {
            process,
            output_tail,
            omitted_bytes,
        }
    }

    pub fn process(&self) -> &ProcessSummary {
        &self.process
    }

    pub fn output_tail(&self) -> &str {
        &self.output_tail
    }

    pub fn omitted_bytes(&self) -> usize {
        self.omitted_bytes
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessOperation {
    OutputAppended {
        content: String,
        truncate_bytes: usize,
        omitted_bytes: usize,
    },
    StatusChanged {
        status: ProcessStatus,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessChange {
    Output,
    Status,
}

impl ProcessOperation {
    pub fn apply(self, state: &mut ProcessState) -> Result<ProcessChange, ApplyError> {
        match self {
            Self::OutputAppended {
                content,
                truncate_bytes,
                omitted_bytes,
            } => {
                if truncate_bytes > state.output_tail.len()
                    || !state.output_tail.is_char_boundary(truncate_bytes)
                {
                    return Err(ApplyError::new("invalid process output truncation"));
                }
                if omitted_bytes < state.omitted_bytes {
                    return Err(ApplyError::new("process omitted byte count decreased"));
                }
                state.output_tail.drain(..truncate_bytes);
                state.output_tail.push_str(&content);
                state.omitted_bytes = omitted_bytes;
                Ok(ProcessChange::Output)
            }
            Self::StatusChanged { status } => {
                if !matches!(state.process.status, ProcessStatus::Running)
                    && matches!(status, ProcessStatus::Running)
                {
                    return Err(ApplyError::new(
                        "terminal process status cannot return to running",
                    ));
                }
                state.process.status = status;
                Ok(ProcessChange::Status)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyError {
    message: &'static str,
}

impl ApplyError {
    fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for ApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl Error for ApplyError {}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Shutdown,
    ThreadCreate {
        display_name: Option<String>,
    },
    ThreadRename {
        thread_id: ThreadId,
        display_name: String,
    },
    ThreadDelete {
        thread_id: ThreadId,
    },
    ThreadDeleteRecursive {
        thread_id: ThreadId,
    },
    ThreadSetModel {
        thread_id: ThreadId,
        provider: String,
        model: String,
        reasoning_effort: String,
    },
    ThreadSend {
        thread_id: ThreadId,
        message: String,
        allow_questions: bool,
    },
    ThreadContinue {
        thread_id: ThreadId,
        allow_questions: bool,
    },
    ThreadCompact {
        thread_id: ThreadId,
        allow_questions: bool,
    },
    ThreadCheckpointCreate {
        thread_id: ThreadId,
    },
    ThreadFork {
        thread_id: ThreadId,
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
        display_name: Option<String>,
    },
    ThreadReplaceHistory {
        thread_id: ThreadId,
        target: HistoryTarget,
    },
    ThreadCancel {
        thread_id: ThreadId,
    },
    ProviderLogin {
        provider: String,
        credential: Option<String>,
    },
    ProviderReloadAuth {
        provider: String,
    },
    ProviderLogout {
        provider: String,
    },
    ApprovalAllow {
        approval_id: InteractionId,
    },
    ApprovalDeny {
        approval_id: InteractionId,
        reason: Option<String>,
    },
    QuestionAnswer {
        request_id: InteractionId,
        answers: Vec<QuestionAnswer>,
    },
    RunnerLaunch {
        name: String,
        description: String,
        approval: ApprovalPolicy,
        command: Vec<String>,
    },
    ExecCommand {
        thread_id: ThreadId,
        runner: String,
        command: String,
    },
    StopProcess {
        process: ProcessLocator,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandResult {
    Accepted,
    ThreadCreated { thread_id: ThreadId },
    ThreadForked { thread_id: ThreadId },
    ProcessStarted { process_id: ProcessId },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandResponse {
    Success { result: CommandResult },
    Error { message: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "resource", rename_all = "snake_case", deny_unknown_fields)]
pub enum Subscribe {
    Controller {},
    Thread { thread_id: ThreadId },
    Checkpoint { checkpoint_id: CheckpointId },
    Process { process: ProcessLocator },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StateRequest {
    Command(Command),
    Subscribe(Subscribe),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubscriptionTerminal {
    Deleted,
    ControllerShutdown,
    Error { message: String },
}

macro_rules! subscription_message {
    ($name:ident, $state:ty, $operation:ty) => {
        #[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
        #[serde(tag = "message", rename_all = "snake_case", deny_unknown_fields)]
        pub enum $name {
            Snapshot { state: $state },
            Operation { operation: $operation },
            Terminal { terminal: SubscriptionTerminal },
        }
    };
}

subscription_message!(
    ControllerSubscriptionMessage,
    ControllerState,
    ControllerOperation
);
subscription_message!(ThreadSubscriptionMessage, ThreadState, ThreadOperation);
subscription_message!(ProcessSubscriptionMessage, ProcessState, ProcessOperation);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "message", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckpointSubscriptionMessage {
    Snapshot { state: CheckpointState },
    Terminal { terminal: SubscriptionTerminal },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessageEvent, ThreadEventData};

    fn thread() -> Thread {
        Thread {
            id: ThreadId(1),
            parent_thread_id: None,
            display_name: None,
            provider: "fake".to_owned(),
            model: "test".to_owned(),
            reasoning_effort: "medium".to_owned(),
        }
    }

    fn event(sequence: i64) -> ThreadEvent {
        ThreadEvent {
            sequence: EventSequence(sequence),
            data: ThreadEventData::UserMessage(MessageEvent {
                content: sequence.to_string(),
            }),
        }
    }

    #[test]
    fn normal_event_updates_are_strictly_append_only() {
        let mut state =
            ThreadState::materialize(thread(), vec![event(2)], Vec::new(), Vec::new()).unwrap();
        ThreadOperation::EventAppended { event: event(3) }
            .apply(&mut state)
            .unwrap();
        assert!(
            ThreadOperation::EventAppended { event: event(3) }
                .apply(&mut state)
                .is_err()
        );
        assert_eq!(state.events().len(), 2);
    }

    #[test]
    fn finalizing_moves_an_active_item_to_events() {
        let mut state =
            ThreadState::materialize(thread(), Vec::new(), Vec::new(), Vec::new()).unwrap();
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(8),
                ActiveItemData::Assistant {
                    content: "partial".to_owned(),
                },
            ),
        }
        .apply(&mut state)
        .unwrap();

        let change = ThreadOperation::ActiveItemFinalized {
            active_id: ActiveItemId(8),
            event: event(1),
        }
        .apply(&mut state)
        .unwrap();

        assert_eq!(
            change,
            ThreadChange::ActiveItemFinalized {
                active_id: ActiveItemId(8),
                sequence: EventSequence(1),
            }
        );
        assert!(state.active_turn().unwrap().items().is_empty());
        assert_eq!(state.events(), &[event(1)]);
    }

    #[test]
    fn cancelling_turn_cannot_resolve_an_approval() {
        let mut state =
            ThreadState::materialize(thread(), Vec::new(), Vec::new(), Vec::new()).unwrap();
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::InteractionRequested {
            interaction: PendingInteraction::Approval(PendingApproval::new(
                InteractionId(1),
                "shell".to_owned(),
                Value::Null,
                None,
                None,
            )),
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::PhaseChanged {
            phase: TurnPhase::Cancelling,
        }
        .apply(&mut state)
        .unwrap();

        assert!(
            ThreadOperation::InteractionResolved {
                interaction_id: InteractionId(1),
            }
            .apply(&mut state)
            .is_err()
        );
        let turn = state.active_turn().unwrap();
        assert_eq!(turn.phase(), TurnPhase::Cancelling);
        assert_eq!(turn.pending_approval().unwrap().id(), InteractionId(1));
    }

    #[test]
    fn question_request_moves_turn_through_awaiting_answer() {
        let mut state =
            ThreadState::materialize(thread(), Vec::new(), Vec::new(), Vec::new()).unwrap();
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        let request_id = InteractionId(7);
        ThreadOperation::InteractionRequested {
            interaction: PendingInteraction::Questions(PendingQuestionRequest {
                id: request_id,
                questions: vec![Question {
                    question: "Choose".to_owned(),
                    options: vec![QuestionOption {
                        label: "A".to_owned(),
                        description: "First".to_owned(),
                    }],
                    recommended_options: vec!["A".to_owned()],
                }],
            }),
        }
        .apply(&mut state)
        .unwrap();

        let turn = state.active_turn().unwrap();
        assert_eq!(turn.phase(), TurnPhase::AwaitingInput);
        assert_eq!(turn.pending_question().unwrap().id, request_id);

        ThreadOperation::InteractionResolved {
            interaction_id: request_id,
        }
        .apply(&mut state)
        .unwrap();
        let turn = state.active_turn().unwrap();
        assert_eq!(turn.phase(), TurnPhase::Running);
        assert!(turn.pending_question().is_none());
    }

    #[test]
    fn retry_status_is_part_of_the_active_turn_state() {
        let mut state =
            ThreadState::materialize(thread(), Vec::new(), Vec::new(), Vec::new()).unwrap();
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::RetryScheduled {
            retry: RetryStatus::new("overloaded".to_owned(), 2, 5),
        }
        .apply(&mut state)
        .unwrap();

        let turn = state.active_turn().unwrap();
        assert_eq!(turn.phase(), TurnPhase::Retrying);
        assert_eq!(turn.retry().unwrap().summary(), "overloaded");
        assert_eq!(turn.retry().unwrap().current(), 2);
        assert_eq!(turn.retry().unwrap().max(), 5);

        ThreadOperation::PhaseChanged {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        assert!(state.active_turn().unwrap().retry().is_none());
    }

    #[test]
    fn terminal_process_cannot_return_to_running() {
        let locator = ProcessLocator::new(
            ThreadId(1),
            "local".to_owned(),
            ProcessId("process-1".to_owned()),
        );
        let summary = ProcessSummary::new(
            locator,
            "echo done".to_owned(),
            1,
            ProcessStatus::Exited { exit_code: Some(0) },
        );
        let mut state = ProcessState::new(summary, "done\n".to_owned(), 0);

        assert!(
            ProcessOperation::StatusChanged {
                status: ProcessStatus::Running,
            }
            .apply(&mut state)
            .is_err()
        );
        assert_eq!(
            state.process().status(),
            &ProcessStatus::Exited { exit_code: Some(0) }
        );
    }

    #[test]
    fn wire_requests_reject_unknown_fields() {
        for request in [
            r#"{"kind":"subscribe","request":{"resource":"controller","extra":true}}"#,
            r#"{"kind":"command","request":{"method":"thread_replace_history","thread_id":1,"target":{"kind":"checkpoint","checkpoint_id":1,"extra":true}}}"#,
        ] {
            assert!(serde_json::from_str::<StateRequest>(request).is_err());
        }
    }

    #[test]
    fn wire_state_messages_reject_unknown_nested_fields() {
        let snapshot = r#"{"message":"snapshot","state":{"lifecycle":"running","threads":[{"id":1,"display_name":null,"provider":"fake","model":"test","reasoning_effort":"medium","extra":true}],"providers":[],"runners":[]}}"#;
        assert!(serde_json::from_str::<ControllerSubscriptionMessage>(snapshot).is_err());

        let operation = r#"{"message":"operation","operation":{"operation":"event_appended","event":{"sequence":1,"kind":"user_message","payload":{"content":"hello","extra":true}}}}"#;
        assert!(serde_json::from_str::<ThreadSubscriptionMessage>(operation).is_err());

        let event = r#"{"message":"operation","operation":{"operation":"event_appended","event":{"sequence":1,"kind":"user_message","payload":{"content":"hello"},"extra":true}}}"#;
        assert!(serde_json::from_str::<ThreadSubscriptionMessage>(event).is_err());
    }
}
