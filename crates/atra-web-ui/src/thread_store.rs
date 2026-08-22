use atra_protocol::{
    ActiveItemData, ActiveItemId, EventSequence, PendingInteraction, ProcessLocator,
    ProcessSummary, RunnerOperationUpdate, Thread, ThreadChange, ThreadCheckpoint, ThreadEventData,
    ThreadState, ThreadSubscriptionMessage,
};
use dioxus::{
    prelude::{Readable, ReadableExt, ReadableRef, Store, WriteSignal},
    stores::scope::SelectorScope,
};

use crate::{
    model::{Diagnostics, RemoteState, latest_diagnostics},
    transcript_view::{
        self, ActivityDisplay, ActivityKey, FinalizedActivities, RawKey, TurnKey, TurnRef,
        raw_item, raw_keys, turn_key_for_event, turn_keys,
    },
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ThreadScope {
    Root,
    Connection,
    Metadata,
    Diagnostics,
    Transcript,
    TranscriptStructure,
    PrettyTranscriptContent,
    RawTranscriptContent,
    RawTranscriptStructure,
    Event(EventSequence),
    Turn(EventSequence),
    ActiveTurnSlot,
    ActiveTurnStatus,
    ActiveItemList,
    ActiveItem(ActiveItemId),
    RunnerCall(String),
    Interaction,
    Outcome,
    Checkpoints,
    Processes,
    Process(ProcessLocator),
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct ThreadStore {
    inner: Store<RemoteState<ThreadState>>,
    finalized: Store<FinalizedActivities>,
}

type ThreadSelector = SelectorScope<WriteSignal<RemoteState<ThreadState>>>;
type StateRead = ReadableRef<'static, ThreadSelector>;
type FinalizedSelector = SelectorScope<WriteSignal<FinalizedActivities>>;
type FinalizedRead = ReadableRef<'static, FinalizedSelector>;

pub(super) struct ConnectionRead(StateRead);
pub(super) struct MetadataRead(StateRead);
pub(super) struct DiagnosticsRead(StateRead);
pub(super) struct TranscriptRead(StateRead);
pub(super) struct RawItemRead {
    state: StateRead,
    key: RawKey,
}
pub(super) struct TurnRead {
    state: StateRead,
    _active_turn: Option<StateRead>,
    _active_items: Option<StateRead>,
    _active_answer: Option<StateRead>,
    key: TurnKey,
}
pub(super) struct ActivityRead {
    state: StateRead,
    runner_call: Option<StateRead>,
    related: Vec<StateRead>,
    key: ActivityKey,
    finalized: FinalizedRead,
}
pub(super) struct ActiveTurnRead(StateRead);
pub(super) struct ActiveRunnerToolsRead(StateRead);
pub(super) struct InteractionRead(StateRead);
pub(super) struct CheckpointsRead(StateRead);
pub(super) struct ProcessesRead(StateRead);
pub(super) struct SnapshotRead(StateRead);

impl ConnectionRead {
    pub fn connected(&self) -> bool {
        self.0.connected
    }
    pub fn terminal(&self) -> Option<&str> {
        self.0.terminal.as_deref()
    }
}

impl MetadataRead {
    pub fn is_loaded(&self) -> bool {
        self.0.value.is_some()
    }
    pub fn metadata(&self) -> Option<&Thread> {
        self.0.value.as_ref().map(ThreadState::metadata)
    }
}

impl DiagnosticsRead {
    pub fn value(&self) -> Option<Diagnostics> {
        self.0.value.as_ref().map(latest_diagnostics)
    }
}

impl TranscriptRead {
    pub fn is_loaded(&self) -> bool {
        self.0.value.is_some()
    }
    pub fn turn_keys(&self) -> Vec<TurnKey> {
        self.0.value.as_ref().map(turn_keys).unwrap_or_default()
    }
    pub fn raw_keys(&self) -> Vec<RawKey> {
        self.0.value.as_ref().map(raw_keys).unwrap_or_default()
    }
}

impl RawItemRead {
    pub fn value(&self) -> Option<String> {
        self.state
            .value
            .as_ref()
            .and_then(|state| raw_item(state, &self.key))
    }
}

impl TurnRead {
    pub fn value(&self) -> Option<TurnRef<'_>> {
        self.state
            .value
            .as_ref()
            .and_then(|state| transcript_view::turn(state, self.key))
    }

    pub fn activity_summary(&self, keys: &[ActivityKey]) -> String {
        self.state
            .value
            .as_ref()
            .map(|state| transcript_view::activity_summary(state, keys))
            .unwrap_or_else(|| "No activity".to_owned())
    }
}

impl ActivityRead {
    pub fn value(&self) -> Option<ActivityDisplay<'_>> {
        self.related
            .iter()
            .rev()
            .chain(self.runner_call.iter())
            .chain(std::iter::once(&self.state))
            .find_map(|state| {
                state
                    .value
                    .as_ref()
                    .and_then(|state| transcript_view::activity(state, &self.key, &self.finalized))
            })
    }
}

impl ActiveTurnRead {
    pub fn is_loaded(&self) -> bool {
        self.0.value.is_some()
    }
    pub fn is_active(&self) -> bool {
        self.0
            .value
            .as_ref()
            .is_some_and(|state| state.active_turn().is_some())
    }
    pub fn is_awaiting_interaction(&self) -> bool {
        self.0
            .value
            .as_ref()
            .and_then(ThreadState::active_turn)
            .is_some_and(|turn| turn.pending_interaction().is_some())
    }
}

impl ActiveRunnerToolsRead {
    pub fn identities(&self, runner: &str) -> Vec<String> {
        self.0
            .value
            .as_ref()
            .and_then(ThreadState::active_turn)
            .into_iter()
            .flat_map(|turn| turn.items())
            .filter_map(|item| match item.data() {
                ActiveItemData::RunnerTool {
                    call_id,
                    operation_index,
                    runner: item_runner,
                    update,
                    ..
                } if item_runner == runner
                    && !matches!(update, RunnerOperationUpdate::Completed { .. }) =>
                {
                    Some(format!("{call_id}:{operation_index}"))
                }
                _ => None,
            })
            .collect()
    }
}

impl InteractionRead {
    pub fn value(&self) -> Option<&PendingInteraction> {
        self.0
            .value
            .as_ref()
            .and_then(ThreadState::active_turn)
            .and_then(|turn| turn.pending_interaction())
    }
}

impl CheckpointsRead {
    pub fn value(&self) -> Option<&[ThreadCheckpoint]> {
        self.0.value.as_ref().map(|state| state.checkpoints())
    }
}

impl ProcessesRead {
    pub fn value(&self) -> Option<&[ProcessSummary]> {
        self.0.value.as_ref().map(|state| state.processes())
    }
}

impl SnapshotRead {
    pub fn is_loaded(&self) -> bool {
        self.0.value.is_some()
    }
    pub fn last_event_sequence(&self) -> Option<EventSequence> {
        self.0
            .value
            .as_ref()?
            .events()
            .last()
            .map(|event| event.sequence)
    }
}

impl ThreadStore {
    pub(super) fn new(
        inner: Store<RemoteState<ThreadState>>,
        finalized: Store<FinalizedActivities>,
    ) -> Self {
        Self { inner, finalized }
    }

    fn scope(self, key: ThreadScope) -> ThreadSelector {
        let root = *self.inner.selector();
        match key {
            ThreadScope::Root => root,
            ThreadScope::Connection => child(root, ScopeSegment::Connection),
            ThreadScope::Metadata => child(root, ScopeSegment::Metadata),
            ThreadScope::Diagnostics => child(root, ScopeSegment::Diagnostics),
            ThreadScope::Transcript => child(root, ScopeSegment::Transcript),
            ThreadScope::TranscriptStructure => child(
                child(root, ScopeSegment::Transcript),
                ScopeSegment::TranscriptStructure,
            ),
            ThreadScope::PrettyTranscriptContent => child(
                child(root, ScopeSegment::Transcript),
                ScopeSegment::PrettyTranscriptContent,
            ),
            ThreadScope::RawTranscriptContent => child(
                child(root, ScopeSegment::Transcript),
                ScopeSegment::RawTranscriptContent,
            ),
            ThreadScope::RawTranscriptStructure => child(
                child(root, ScopeSegment::Transcript),
                ScopeSegment::RawTranscriptStructure,
            ),
            ThreadScope::Event(sequence) => child(
                child(root, ScopeSegment::Transcript),
                ScopeSegment::Event(sequence),
            ),
            ThreadScope::Turn(sequence) => child(
                child(root, ScopeSegment::Transcript),
                ScopeSegment::Turn(sequence),
            ),
            ThreadScope::ActiveTurnSlot => child(
                child(root, ScopeSegment::Transcript),
                ScopeSegment::ActiveTurnSlot,
            ),
            ThreadScope::ActiveTurnStatus => child(
                child(
                    child(root, ScopeSegment::Transcript),
                    ScopeSegment::ActiveTurnSlot,
                ),
                ScopeSegment::ActiveTurnStatus,
            ),
            ThreadScope::ActiveItemList => child(
                child(
                    child(root, ScopeSegment::Transcript),
                    ScopeSegment::ActiveTurnSlot,
                ),
                ScopeSegment::ActiveItemList,
            ),
            ThreadScope::ActiveItem(id) => child(
                // Keep item content outside ActiveTurnSlot: deep slot subscribers
                // observe lifecycle/list changes, not every streaming text delta.
                child(root, ScopeSegment::Transcript),
                ScopeSegment::ActiveItem(id),
            ),
            ThreadScope::RunnerCall(call_id) => child(
                child(root, ScopeSegment::Transcript),
                ScopeSegment::RunnerCall(call_id),
            ),
            ThreadScope::Interaction => child(
                child(
                    child(root, ScopeSegment::Transcript),
                    ScopeSegment::ActiveTurnSlot,
                ),
                ScopeSegment::Interaction,
            ),
            ThreadScope::Outcome => {
                child(child(root, ScopeSegment::Transcript), ScopeSegment::Outcome)
            }
            ThreadScope::Checkpoints => child(root, ScopeSegment::Checkpoints),
            ThreadScope::Processes => child(root, ScopeSegment::Processes),
            ThreadScope::Process(locator) => child(
                child(root, ScopeSegment::Processes),
                ScopeSegment::Process(locator),
            ),
        }
    }

    fn read(self, key: ThreadScope) -> ReadableRef<'static, ThreadSelector> {
        self.scope(key)
            .try_read_unchecked()
            .expect("thread store must be readable")
    }

    fn peek(self, key: ThreadScope) -> ReadableRef<'static, ThreadSelector> {
        self.scope(key)
            .try_peek_unchecked()
            .expect("thread store must be readable")
    }

    pub(super) fn read_snapshot(self) -> SnapshotRead {
        SnapshotRead(self.read(ThreadScope::Root))
    }

    pub(super) fn read_connection(self) -> ConnectionRead {
        ConnectionRead(self.read(ThreadScope::Connection))
    }

    pub(super) fn read_metadata(self) -> MetadataRead {
        MetadataRead(self.read(ThreadScope::Metadata))
    }

    pub(super) fn read_diagnostics(self) -> DiagnosticsRead {
        DiagnosticsRead(self.read(ThreadScope::Diagnostics))
    }

    pub(super) fn read_transcript_structure(self) -> TranscriptRead {
        TranscriptRead(self.read(ThreadScope::TranscriptStructure))
    }

    pub(super) fn read_raw_transcript(self) -> TranscriptRead {
        TranscriptRead(self.read(ThreadScope::RawTranscriptStructure))
    }

    pub(super) fn read_raw_item(self, key: RawKey) -> RawItemRead {
        let state = match key {
            RawKey::Event(sequence) => self.read(ThreadScope::Event(sequence)),
            RawKey::Active(id) => self.read(ThreadScope::ActiveItem(id)),
        };
        RawItemRead { state, key }
    }

    pub(super) fn read_turn(self, key: TurnKey) -> TurnRead {
        let state = self.read(ThreadScope::Turn(key.sequence()));
        let can_have_active = state
            .value
            .as_ref()
            .and_then(|state| transcript_view::turn(state, key))
            .is_some_and(|turn| turn.can_have_active());
        let active_answer = can_have_active
            .then(|| {
                state
                    .value
                    .as_ref()?
                    .active_turn()?
                    .items()
                    .iter()
                    .find(|item| matches!(item.data(), ActiveItemData::Assistant { .. }))
                    .map(|item| item.id())
            })
            .flatten();
        TurnRead {
            state,
            _active_turn: can_have_active.then(|| self.read(ThreadScope::ActiveTurnSlot)),
            _active_items: can_have_active.then(|| self.read(ThreadScope::ActiveItemList)),
            _active_answer: active_answer.map(|id| self.read(ThreadScope::ActiveItem(id))),
            key,
        }
    }

    pub(super) fn peek_turn(self, key: TurnKey) -> TurnRead {
        TurnRead {
            state: self.peek(ThreadScope::Turn(key.sequence())),
            _active_turn: None,
            _active_items: None,
            _active_answer: None,
            key,
        }
    }

    pub(super) fn read_activity(self, turn: TurnKey, key: ActivityKey) -> ActivityRead {
        // The finalized map is consulted without subscribing: an entry is
        // only added together with the ActiveItem/Turn scope invalidation
        // that already re-renders the activity reader, so a subscription
        // would only add re-renders without changing the result.
        let finalized = self
            .finalized
            .selector()
            .try_peek_unchecked()
            .expect("finalized store must be readable");
        let resolved = self
            .inner
            .peek()
            .value
            .as_ref()
            .map(|state| transcript_view::resolve_activity_key(state, &key, &finalized))
            .unwrap_or_else(|| key.clone());
        let state = match resolved {
            ActivityKey::Active(id) | ActivityKey::StableActive { id, .. } => {
                self.read(ThreadScope::ActiveItem(id))
            }
            _ => self.read(ThreadScope::Turn(turn.sequence())),
        };
        let tracks_runner_call = self.inner.peek().value.as_ref().is_some_and(|state| {
            transcript_view::activity_can_receive_active_updates(state, &resolved, &finalized)
        });
        let runner_call = tracks_runner_call
            .then(|| match &resolved {
                ActivityKey::Tool {
                    identity: Some(call_id),
                    ..
                } => Some(call_id.clone()),
                _ => None,
            })
            .flatten();
        let related = runner_call
            .as_deref()
            .and_then(|call_id| {
                self.inner
                    .peek()
                    .value
                    .as_ref()?
                    .active_turn()
                    .map(|active| {
                        active
                            .items()
                            .iter()
                            .filter_map(|item| match item.data() {
                                ActiveItemData::RunnerTool {
                                    call_id: item_call_id,
                                    ..
                                } if item_call_id == call_id => {
                                    Some(self.read(ThreadScope::ActiveItem(item.id())))
                                }
                                _ => None,
                            })
                            .collect()
                    })
            })
            .unwrap_or_default();
        ActivityRead {
            state,
            runner_call: runner_call.map(|call_id| self.read(ThreadScope::RunnerCall(call_id))),
            related,
            key,
            finalized,
        }
    }

    pub(super) fn read_active_turn(self) -> ActiveTurnRead {
        ActiveTurnRead(self.read(ThreadScope::ActiveTurnSlot))
    }

    pub(super) fn read_active_runner_tools(self) -> ActiveRunnerToolsRead {
        ActiveRunnerToolsRead(self.read(ThreadScope::Root))
    }

    pub(super) fn read_interaction(self) -> InteractionRead {
        InteractionRead(self.read(ThreadScope::Interaction))
    }

    pub(super) fn read_checkpoints(self) -> CheckpointsRead {
        CheckpointsRead(self.read(ThreadScope::Checkpoints))
    }

    pub(super) fn read_processes(self) -> ProcessesRead {
        ProcessesRead(self.read(ThreadScope::Processes))
    }

    pub(super) fn track_pretty_transcript_content(self) {
        let _subscription = self.read(ThreadScope::PrettyTranscriptContent);
    }

    pub(super) fn track_raw_transcript_content(self) {
        let _subscription = self.read(ThreadScope::RawTranscriptContent);
    }

    pub(super) fn apply(self, message: ThreadSubscriptionMessage) {
        match message {
            ThreadSubscriptionMessage::Snapshot { state } => {
                {
                    let sequences: std::collections::HashSet<_> =
                        state.events().iter().map(|event| event.sequence).collect();
                    let mut finalized = self.finalized.selector().write_untracked();
                    finalized
                        .by_active_id
                        .retain(|_, sequence| sequences.contains(sequence));
                }
                {
                    let mut remote = self.inner.selector().write_untracked();
                    remote.value = Some(state);
                    remote.connected = true;
                    remote.terminal = None;
                }
                self.scope(ThreadScope::Root).mark_dirty();
            }
            ThreadSubscriptionMessage::Operation { operation } => {
                let change = {
                    let mut remote = self.inner.selector().write_untracked();
                    let result = remote
                        .value
                        .as_mut()
                        .ok_or_else(|| "operation arrived before snapshot".to_owned())
                        .and_then(|state| {
                            operation.apply(state).map_err(|error| error.to_string())
                        });
                    match result {
                        Ok(change) => Some(change),
                        Err(error) => {
                            remote.connected = false;
                            remote.terminal = Some(error);
                            None
                        }
                    }
                };
                match change {
                    Some(change) => self.invalidate(change),
                    None => self.scope(ThreadScope::Connection).mark_dirty(),
                }
            }
            ThreadSubscriptionMessage::Terminal { terminal } => {
                {
                    let mut remote = self.inner.selector().write_untracked();
                    remote.connected = false;
                    remote.terminal = Some(format!("{terminal:?}"));
                }
                self.scope(ThreadScope::Connection).mark_dirty();
            }
        }
    }

    pub(super) fn set_connected(self, connected: bool) {
        {
            let mut remote = self.inner.selector().write_untracked();
            remote.connected = connected;
        }
        self.scope(ThreadScope::Connection).mark_dirty();
    }

    fn invalidate(self, change: ThreadChange) {
        if matches!(
            change,
            ThreadChange::EventAppended(_)
                | ThreadChange::ActiveItemAdded(_)
                | ThreadChange::ActiveItemUpdated(_)
                | ThreadChange::ActiveItemRemoved(_)
                | ThreadChange::ActiveItemFinalized { .. }
                | ThreadChange::ToolResultFinalized { .. }
                | ThreadChange::TurnFinished
        ) {
            self.scope(ThreadScope::RawTranscriptContent)
                .mark_dirty_shallow();
        }
        if matches!(
            change,
            ThreadChange::EventAppended(_)
                | ThreadChange::ActiveItemAdded(_)
                | ThreadChange::ActiveItemRemoved(_)
                | ThreadChange::ActiveItemFinalized { .. }
                | ThreadChange::ToolResultFinalized { .. }
                | ThreadChange::TurnFinished
        ) {
            self.scope(ThreadScope::RawTranscriptStructure)
                .mark_dirty_shallow();
        }
        match change {
            ThreadChange::MetadataUpdated => self.scope(ThreadScope::Metadata).mark_dirty(),
            ThreadChange::EventAppended(sequence) => {
                let targets = {
                    let remote = self.inner.peek();
                    remote
                        .value
                        .as_ref()
                        .map(|state| event_targets(state, sequence))
                        .unwrap_or_default()
                };
                let updates_pretty = targets.iter().any(|target| {
                    matches!(
                        target,
                        ThreadScope::TranscriptStructure | ThreadScope::Turn(_)
                    )
                });
                for target in targets {
                    self.scope(target).mark_dirty();
                }
                if updates_pretty {
                    self.scope(ThreadScope::PrettyTranscriptContent)
                        .mark_dirty_shallow();
                }
            }
            ThreadChange::EventsReplaced => {
                {
                    let sequences = self
                        .inner
                        .peek()
                        .value
                        .as_ref()
                        .map(|state| {
                            state
                                .events()
                                .iter()
                                .map(|event| event.sequence)
                                .collect::<std::collections::HashSet<_>>()
                        })
                        .unwrap_or_default();
                    let mut finalized = self.finalized.selector().write_untracked();
                    finalized
                        .by_active_id
                        .retain(|_, sequence| sequences.contains(sequence));
                }
                self.scope(ThreadScope::Transcript).mark_dirty();
                self.scope(ThreadScope::Diagnostics).mark_dirty();
            }
            ThreadChange::ActiveTurnStarted => {
                self.scope(ThreadScope::ActiveTurnSlot).mark_dirty();
                self.scope(ThreadScope::Outcome).mark_dirty();
                self.scope(ThreadScope::PrettyTranscriptContent)
                    .mark_dirty_shallow();
            }
            ThreadChange::ActiveTurnStateUpdated => {
                self.scope(ThreadScope::ActiveTurnStatus).mark_dirty();
            }
            ThreadChange::ActiveItemAdded(id) => {
                self.scope(ThreadScope::ActiveItemList).mark_dirty();
                let runner_call = {
                    let remote = self.inner.peek();
                    remote.value.as_ref().and_then(|state| {
                        state
                            .active_turn()
                            .into_iter()
                            .flat_map(|turn| turn.items())
                            .find(|item| item.id() == id)
                            .and_then(|item| match item.data() {
                                ActiveItemData::RunnerTool { call_id, .. } => Some(call_id.clone()),
                                _ => None,
                            })
                    })
                };
                if let Some(call_id) = runner_call {
                    self.scope(ThreadScope::RunnerCall(call_id)).mark_dirty();
                }
                self.scope(ThreadScope::PrettyTranscriptContent)
                    .mark_dirty_shallow();
            }
            ThreadChange::ActiveItemRemoved(id) => {
                self.scope(ThreadScope::ActiveItemList).mark_dirty();
                self.scope(ThreadScope::ActiveItem(id)).mark_dirty();
                self.scope(ThreadScope::PrettyTranscriptContent)
                    .mark_dirty_shallow();
            }
            ThreadChange::ActiveItemUpdated(id) => {
                self.scope(ThreadScope::ActiveItem(id)).mark_dirty();
                self.scope(ThreadScope::PrettyTranscriptContent)
                    .mark_dirty_shallow();
            }
            ThreadChange::ActiveItemFinalized {
                active_id,
                sequence,
            } => {
                {
                    let mut finalized = self.finalized.selector().write_untracked();
                    finalized.by_active_id.insert(active_id, sequence);
                }
                self.scope(ThreadScope::ActiveItemList).mark_dirty();
                self.scope(ThreadScope::ActiveItem(active_id)).mark_dirty();
                let target = {
                    let remote = self.inner.peek();
                    remote.value.as_ref().and_then(|state| {
                        turn_key_for_event(state, sequence)
                            .map(|key| ThreadScope::Turn(key.sequence()))
                    })
                };
                if let Some(target) = target {
                    self.scope(target).mark_dirty();
                }
                self.scope(ThreadScope::PrettyTranscriptContent)
                    .mark_dirty_shallow();
            }
            ThreadChange::ToolResultFinalized {
                sequence,
                runner_ids,
            } => {
                self.scope(ThreadScope::ActiveItemList).mark_dirty();
                for id in runner_ids {
                    self.scope(ThreadScope::ActiveItem(id)).mark_dirty();
                }
                let target = {
                    let remote = self.inner.peek();
                    remote.value.as_ref().and_then(|state| {
                        turn_key_for_event(state, sequence)
                            .map(|key| ThreadScope::Turn(key.sequence()))
                    })
                };
                if let Some(target) = target {
                    self.scope(target).mark_dirty();
                }
                self.scope(ThreadScope::PrettyTranscriptContent)
                    .mark_dirty_shallow();
            }
            ThreadChange::InteractionUpdated => {
                self.scope(ThreadScope::Interaction).mark_dirty();
                self.scope(ThreadScope::ActiveTurnStatus).mark_dirty();
                self.scope(ThreadScope::PrettyTranscriptContent)
                    .mark_dirty_shallow();
            }
            ThreadChange::TurnFinished => {
                self.scope(ThreadScope::ActiveTurnSlot).mark_dirty();
                self.scope(ThreadScope::Outcome).mark_dirty();
                self.scope(ThreadScope::PrettyTranscriptContent)
                    .mark_dirty_shallow();
            }
            ThreadChange::CheckpointAdded(_) => {
                self.scope(ThreadScope::Checkpoints).mark_dirty();
            }
            ThreadChange::ProcessUpdated(locator) => {
                self.scope(ThreadScope::Process(locator)).mark_dirty();
                self.scope(ThreadScope::Processes).mark_dirty_shallow();
            }
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ScopeSegment {
    Connection,
    Metadata,
    Diagnostics,
    Transcript,
    TranscriptStructure,
    PrettyTranscriptContent,
    RawTranscriptContent,
    RawTranscriptStructure,
    Event(EventSequence),
    Turn(EventSequence),
    ActiveTurnSlot,
    ActiveTurnStatus,
    ActiveItemList,
    ActiveItem(ActiveItemId),
    RunnerCall(String),
    Interaction,
    Outcome,
    Checkpoints,
    Processes,
    Process(ProcessLocator),
}

fn child(scope: ThreadSelector, segment: ScopeSegment) -> ThreadSelector {
    scope.hash_child_unmapped(&segment)
}

fn event_targets(state: &ThreadState, sequence: EventSequence) -> Vec<ThreadScope> {
    let Some(event) = state
        .events()
        .last()
        .filter(|event| event.sequence == sequence)
    else {
        return Vec::new();
    };
    match &event.data {
        ThreadEventData::UserMessage(_) => vec![ThreadScope::TranscriptStructure],
        ThreadEventData::AssistantMessage(_)
        | ThreadEventData::ToolCall(_)
        | ThreadEventData::ToolResult(_)
        | ThreadEventData::Reasoning(_)
        | ThreadEventData::WebSearch(_)
        | ThreadEventData::SkillInvocation(_)
        | ThreadEventData::Compaction(_)
        | ThreadEventData::ApprovalDecision(_)
        | ThreadEventData::Retry(_)
        | ThreadEventData::TurnOutcome(_) => turn_key_for_event(state, sequence)
            .map(|key| ThreadScope::Turn(key.sequence()))
            .into_iter()
            .collect(),
        ThreadEventData::TokenUsage(_) | ThreadEventData::RateLimits(_) => {
            vec![ThreadScope::Diagnostics]
        }
        ThreadEventData::ThreadContext(_)
        | ThreadEventData::WorkspaceInstructions(_)
        | ThreadEventData::Skills(_)
        | ThreadEventData::Runners(_)
        | ThreadEventData::ModelRequest(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use dioxus::prelude::*;
    use serde_json::json;

    use super::*;

    #[derive(Clone, Default, PartialEq)]
    struct RenderCounts {
        metadata: usize,
        diagnostics: usize,
        transcript_structure: usize,
        raw_transcript_structure: usize,
        turn: usize,
        active_turn_slot: usize,
        item_list: usize,
        item: usize,
        raw_item: usize,
        transcript_content: usize,
        raw_transcript_content: usize,
    }

    #[derive(Clone, PartialEq)]
    struct Harness {
        store: Rc<RefCell<Option<ThreadStore>>>,
        counts: Rc<RefCell<RenderCounts>>,
    }

    #[component]
    fn MetadataReader(store: ThreadStore, counts: Rc<RefCell<RenderCounts>>) -> Element {
        counts.borrow_mut().metadata += 1;
        let scope = store.scope(ThreadScope::Metadata);
        let _subscription = scope.read();
        rsx! {}
    }

    #[component]
    fn DiagnosticsReader(store: ThreadStore, counts: Rc<RefCell<RenderCounts>>) -> Element {
        counts.borrow_mut().diagnostics += 1;
        let _diagnostics = store.read_diagnostics();
        rsx! {}
    }

    #[component]
    fn ItemListReader(store: ThreadStore, counts: Rc<RefCell<RenderCounts>>) -> Element {
        counts.borrow_mut().item_list += 1;
        let scope = store.scope(ThreadScope::ActiveItemList);
        let _subscription = scope.read();
        rsx! {}
    }

    #[component]
    fn TranscriptStructureReader(store: ThreadStore, counts: Rc<RefCell<RenderCounts>>) -> Element {
        counts.borrow_mut().transcript_structure += 1;
        let scope = store.scope(ThreadScope::TranscriptStructure);
        let _subscription = scope.read();
        rsx! {}
    }

    #[component]
    fn RawTranscriptStructureReader(
        store: ThreadStore,
        counts: Rc<RefCell<RenderCounts>>,
    ) -> Element {
        counts.borrow_mut().raw_transcript_structure += 1;
        let _structure = store.read_raw_transcript();
        rsx! {}
    }

    #[component]
    fn TurnReader(store: ThreadStore, counts: Rc<RefCell<RenderCounts>>) -> Element {
        counts.borrow_mut().turn += 1;
        let scope = store.scope(ThreadScope::Turn(EventSequence(1)));
        let _subscription = scope.read();
        rsx! {}
    }

    #[component]
    fn ActiveTurnSlotReader(store: ThreadStore, counts: Rc<RefCell<RenderCounts>>) -> Element {
        counts.borrow_mut().active_turn_slot += 1;
        let scope = store.scope(ThreadScope::ActiveTurnSlot);
        let _subscription = scope.read();
        rsx! {}
    }

    #[component]
    fn ItemReader(store: ThreadStore, counts: Rc<RefCell<RenderCounts>>) -> Element {
        counts.borrow_mut().item += 1;
        let scope = store.scope(ThreadScope::ActiveItem(ActiveItemId(9)));
        let _subscription = scope.read();
        rsx! {}
    }

    #[component]
    fn RawItemReader(store: ThreadStore, counts: Rc<RefCell<RenderCounts>>) -> Element {
        counts.borrow_mut().raw_item += 1;
        let _item = store.read_raw_item(RawKey::Active(ActiveItemId(9)));
        rsx! {}
    }

    #[component]
    fn TranscriptContentReader(store: ThreadStore, counts: Rc<RefCell<RenderCounts>>) -> Element {
        counts.borrow_mut().transcript_content += 1;
        let scope = store.scope(ThreadScope::PrettyTranscriptContent);
        let _subscription = scope.read();
        rsx! {}
    }

    #[component]
    fn RawTranscriptContentReader(
        store: ThreadStore,
        counts: Rc<RefCell<RenderCounts>>,
    ) -> Element {
        counts.borrow_mut().raw_transcript_content += 1;
        let scope = store.scope(ThreadScope::RawTranscriptContent);
        let _subscription = scope.read();
        rsx! {}
    }

    fn app(harness: Harness) -> Element {
        let store = ThreadStore::new(
            use_store(RemoteState::<ThreadState>::default),
            use_store(FinalizedActivities::default),
        );
        *harness.store.borrow_mut() = Some(store);
        rsx! {
            MetadataReader { store, counts: harness.counts.clone() }
            DiagnosticsReader { store, counts: harness.counts.clone() }
            TranscriptStructureReader { store, counts: harness.counts.clone() }
            RawTranscriptStructureReader { store, counts: harness.counts.clone() }
            TurnReader { store, counts: harness.counts.clone() }
            ActiveTurnSlotReader { store, counts: harness.counts.clone() }
            ItemListReader { store, counts: harness.counts.clone() }
            ItemReader { store, counts: harness.counts.clone() }
            RawItemReader { store, counts: harness.counts.clone() }
            TranscriptContentReader { store, counts: harness.counts.clone() }
            RawTranscriptContentReader { store, counts: harness.counts }
        }
    }

    fn message(value: serde_json::Value) -> ThreadSubscriptionMessage {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn active_text_update_rerenders_only_the_item_and_transcript_content_observer() {
        let harness = Harness {
            store: Rc::new(RefCell::new(None)),
            counts: Rc::new(RefCell::new(RenderCounts::default())),
        };
        let mut dom = VirtualDom::new_with_props(app, harness.clone());
        dom.rebuild_in_place();
        let store = harness.store.borrow().unwrap();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "snapshot",
                "state": {
                    "metadata": {
                        "id": 1,
                        "parent_thread_id": null,
                        "display_name": "Thread",
                        "provider": "fake",
                        "model": "model",
                        "reasoning_effort": "medium"
                    },
                    "events": [
                        {"sequence": 1, "kind": "user_message", "payload": {"content": "prompt"}}
                    ],
                    "active_turn": {
                        "phase": "running",
                        "items": [{
                            "id": 9,
                            "data": {"kind": "assistant", "content": "Streaming", "phase": "final_answer"}
                        }],
                        "pending_interaction": null
                    },
                    "last_outcome": null,
                    "checkpoints": [],
                    "processes": []
                }
            })));
        });
        dom.render_immediate_to_vec();
        let baseline = harness.counts.borrow().clone();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "operation",
                "operation": {
                    "operation": "active_text_appended",
                    "id": 9,
                    "content": " update"
                }
            })));
        });
        dom.render_immediate_to_vec();

        let counts = harness.counts.borrow();
        assert_eq!(counts.metadata, baseline.metadata);
        assert_eq!(counts.transcript_structure, baseline.transcript_structure);
        assert_eq!(
            counts.raw_transcript_structure,
            baseline.raw_transcript_structure
        );
        assert_eq!(counts.turn, baseline.turn);
        assert_eq!(counts.active_turn_slot, baseline.active_turn_slot);
        assert_eq!(counts.item_list, baseline.item_list);
        assert_eq!(counts.item, baseline.item + 1);
        assert_eq!(counts.raw_item, baseline.raw_item + 1);
        assert_eq!(counts.transcript_content, baseline.transcript_content + 1);
        assert_eq!(
            counts.raw_transcript_content,
            baseline.raw_transcript_content + 1
        );
    }

    #[test]
    fn appended_activity_rerenders_the_owning_turn_without_rebuilding_structure() {
        let harness = Harness {
            store: Rc::new(RefCell::new(None)),
            counts: Rc::new(RefCell::new(RenderCounts::default())),
        };
        let mut dom = VirtualDom::new_with_props(app, harness.clone());
        dom.rebuild_in_place();
        let store = harness.store.borrow().unwrap();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "snapshot",
                "state": {
                    "metadata": {
                        "id": 1,
                        "parent_thread_id": null,
                        "display_name": "Thread",
                        "provider": "fake",
                        "model": "model",
                        "reasoning_effort": "medium"
                    },
                    "events": [
                        {"sequence": 1, "kind": "user_message", "payload": {"content": "prompt"}}
                    ],
                    "active_turn": null,
                    "last_outcome": null,
                    "checkpoints": [],
                    "processes": []
                }
            })));
        });
        dom.render_immediate_to_vec();
        let baseline = harness.counts.borrow().clone();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "operation",
                "operation": {
                    "operation": "event_appended",
                    "event": {
                        "sequence": 2,
                        "kind": "assistant_message",
                        "payload": {"content": "working", "phase": "commentary"}
                    }
                }
            })));
        });
        dom.render_immediate_to_vec();

        let counts = harness.counts.borrow();
        assert_eq!(counts.metadata, baseline.metadata);
        assert_eq!(counts.transcript_structure, baseline.transcript_structure);
        assert_eq!(
            counts.raw_transcript_structure,
            baseline.raw_transcript_structure + 1
        );
        assert_eq!(counts.turn, baseline.turn + 1);
        assert_eq!(counts.active_turn_slot, baseline.active_turn_slot);
        assert_eq!(counts.item_list, baseline.item_list);
        assert_eq!(counts.item, baseline.item);
        assert_eq!(counts.transcript_content, baseline.transcript_content + 1);
        assert_eq!(
            counts.raw_transcript_content,
            baseline.raw_transcript_content + 1
        );
    }

    #[test]
    fn appended_activity_rerenders_the_compaction_anchored_turn() {
        let harness = Harness {
            store: Rc::new(RefCell::new(None)),
            counts: Rc::new(RefCell::new(RenderCounts::default())),
        };
        let mut dom = VirtualDom::new_with_props(app, harness.clone());
        dom.rebuild_in_place();
        let store = harness.store.borrow().unwrap();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "snapshot",
                "state": {
                    "metadata": {
                        "id": 1,
                        "parent_thread_id": null,
                        "display_name": "Thread",
                        "provider": "fake",
                        "model": "model",
                        "reasoning_effort": "medium"
                    },
                    "events": [{
                        "sequence": 1,
                        "kind": "compaction",
                        "payload": {"replacement": {"type": "summary", "content": "Earlier summary"}, "through": 0}
                    }],
                    "active_turn": null,
                    "last_outcome": null,
                    "checkpoints": [],
                    "processes": []
                }
            })));
        });
        dom.render_immediate_to_vec();
        let baseline = harness.counts.borrow().clone();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "operation",
                "operation": {
                    "operation": "event_appended",
                    "event": {
                        "sequence": 2,
                        "kind": "assistant_message",
                        "payload": {"content": "working", "phase": "commentary"}
                    }
                }
            })));
        });
        dom.render_immediate_to_vec();

        let counts = harness.counts.borrow();
        assert_eq!(counts.transcript_structure, baseline.transcript_structure);
        assert_eq!(counts.turn, baseline.turn + 1);
        assert_eq!(counts.transcript_content, baseline.transcript_content + 1);
    }

    #[test]
    fn starting_a_turn_does_not_rebuild_transcript_structures() {
        let harness = Harness {
            store: Rc::new(RefCell::new(None)),
            counts: Rc::new(RefCell::new(RenderCounts::default())),
        };
        let mut dom = VirtualDom::new_with_props(app, harness.clone());
        dom.rebuild_in_place();
        let store = harness.store.borrow().unwrap();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "snapshot",
                "state": {
                    "metadata": {
                        "id": 1,
                        "parent_thread_id": null,
                        "display_name": "Thread",
                        "provider": "fake",
                        "model": "model",
                        "reasoning_effort": "medium"
                    },
                    "events": [
                        {"sequence": 1, "kind": "user_message", "payload": {"content": "prompt"}}
                    ],
                    "active_turn": null,
                    "last_outcome": null,
                    "checkpoints": [],
                    "processes": []
                }
            })));
        });
        dom.render_immediate_to_vec();
        let baseline = harness.counts.borrow().clone();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "operation",
                "operation": {
                    "operation": "active_turn_started",
                    "phase": "running"
                }
            })));
        });
        dom.render_immediate_to_vec();

        let counts = harness.counts.borrow();
        assert_eq!(counts.transcript_structure, baseline.transcript_structure);
        assert_eq!(
            counts.raw_transcript_structure,
            baseline.raw_transcript_structure
        );
        assert_eq!(counts.active_turn_slot, baseline.active_turn_slot + 1);
    }

    #[test]
    fn adding_an_active_item_rebuilds_only_raw_structure_and_active_turn_content() {
        let harness = Harness {
            store: Rc::new(RefCell::new(None)),
            counts: Rc::new(RefCell::new(RenderCounts::default())),
        };
        let mut dom = VirtualDom::new_with_props(app, harness.clone());
        dom.rebuild_in_place();
        let store = harness.store.borrow().unwrap();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "snapshot",
                "state": {
                    "metadata": {
                        "id": 1,
                        "parent_thread_id": null,
                        "display_name": "Thread",
                        "provider": "fake",
                        "model": "model",
                        "reasoning_effort": "medium"
                    },
                    "events": [
                        {"sequence": 1, "kind": "user_message", "payload": {"content": "prompt"}}
                    ],
                    "active_turn": {
                        "phase": "running",
                        "items": [],
                        "pending_interaction": null
                    },
                    "last_outcome": null,
                    "checkpoints": [],
                    "processes": []
                }
            })));
        });
        dom.render_immediate_to_vec();
        let baseline = harness.counts.borrow().clone();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "operation",
                "operation": {
                    "operation": "active_item_added",
                    "item": {
                        "id": 9,
                        "data": {"kind": "assistant", "content": "Streaming", "phase": "final_answer"}
                    }
                }
            })));
        });
        dom.render_immediate_to_vec();

        let counts = harness.counts.borrow();
        assert_eq!(counts.transcript_structure, baseline.transcript_structure);
        assert_eq!(
            counts.raw_transcript_structure,
            baseline.raw_transcript_structure + 1
        );
        assert_eq!(counts.item_list, baseline.item_list + 1);
        assert_eq!(counts.item, baseline.item);
        assert_eq!(counts.raw_item, baseline.raw_item);
    }

    #[test]
    fn replacing_events_rerenders_diagnostics() {
        let harness = Harness {
            store: Rc::new(RefCell::new(None)),
            counts: Rc::new(RefCell::new(RenderCounts::default())),
        };
        let mut dom = VirtualDom::new_with_props(app, harness.clone());
        dom.rebuild_in_place();
        let store = harness.store.borrow().unwrap();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "snapshot",
                "state": {
                    "metadata": {
                        "id": 1,
                        "parent_thread_id": null,
                        "display_name": "Thread",
                        "provider": "fake",
                        "model": "model",
                        "reasoning_effort": "medium"
                    },
                    "events": [
                        {"sequence": 1, "kind": "user_message", "payload": {"content": "prompt"}},
                        {
                            "sequence": 2,
                            "kind": "token_usage",
                            "payload": {
                                "request_sequence": 1,
                                "usage": {
                                    "input_tokens": 10,
                                    "cached_input_tokens": 0,
                                    "output_tokens": 5,
                                    "total_tokens": 15
                                }
                            }
                        }
                    ],
                    "active_turn": null,
                    "last_outcome": null,
                    "checkpoints": [],
                    "processes": []
                }
            })));
        });
        dom.render_immediate_to_vec();
        let baseline = harness.counts.borrow().clone();

        dom.in_scope(ScopeId::APP, || {
            store.apply(message(json!({
                "message": "operation",
                "operation": {
                    "operation": "events_replaced",
                    "events": [
                        {"sequence": 1, "kind": "user_message", "payload": {"content": "prompt"}}
                    ]
                }
            })));
        });
        dom.render_immediate_to_vec();

        let counts = harness.counts.borrow();
        assert_eq!(counts.diagnostics, baseline.diagnostics + 1);
    }
}
