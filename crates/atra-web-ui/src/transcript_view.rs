use std::fmt;
use std::{borrow::Cow, collections::HashMap};

use crate::diff_view::{
    DiffViewFile, DiffViewHunk, DiffViewKind, DiffViewLine, DiffViewLineKind, DiffViewStatus,
    SnapshotDiff,
};
use crate::model::pretty;
use atra_patch_types::{
    ApplyPatchResult, DiffLineKind, FileDiff, PatchOperationOutcome, PatchOperationResult,
};
use atra_protocol::{
    ActiveItem, ActiveItemData, ActiveItemId, AssistantMessagePhase, CommandExecutionArtifact,
    EventSequence, RunnerOperationUpdate, ThreadEvent, ThreadEventData, ThreadState, TodoStatus,
    ToolArtifact, ToolCallEvent, ToolResultEvent,
};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) struct TurnKey(EventSequence);

impl TurnKey {
    pub fn sequence(self) -> EventSequence {
        self.0
    }
}

impl fmt::Display for TurnKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "turn-{}", self.0.0)
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) enum ActivityKey {
    Event(EventSequence),
    Tool {
        call: EventSequence,
        identity: Option<String>,
    },
    Todo {
        source: EventSequence,
    },
    Active(ActiveItemId),
    StableActive {
        id: ActiveItemId,
        identity: String,
    },
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) enum RawKey {
    Event(EventSequence),
    Active(ActiveItemId),
}

/// Maps active items to the transcript events they were finalized into,
/// so a selection made while an item was streaming keeps resolving after
/// the item is replaced by its final event.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct FinalizedActivities {
    pub(super) by_active_id: HashMap<ActiveItemId, EventSequence>,
}

impl fmt::Display for RawKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Event(sequence) => write!(formatter, "raw-event-{}", sequence.0),
            Self::Active(id) => write!(formatter, "raw-active-{}", id.0),
        }
    }
}

impl fmt::Display for ActivityKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Event(sequence) => write!(formatter, "event-{}", sequence.0),
            Self::Tool { call, identity } => match identity {
                Some(identity) => write!(formatter, "tool-{identity}"),
                None => write!(formatter, "tool-{}", call.0),
            },
            Self::Todo { source } => write!(formatter, "todo-{}", source.0),
            Self::Active(id) => write!(formatter, "active-{}", id.0),
            Self::StableActive { identity, .. } => write!(formatter, "tool-{identity}"),
        }
    }
}

pub(super) struct TurnRef<'a> {
    state: &'a ThreadState,
    start: usize,
    end: usize,
    prompt: &'a str,
    prompt_sequence: Option<EventSequence>,
}

pub(super) enum ActivityDisplay<'a> {
    Commentary {
        markdown: &'a str,
    },
    Todo {
        items: &'a [atra_protocol::TodoItem],
    },
    Reasoning {
        summary: Cow<'a, str>,
    },
    Command(CommandDisplay),
    Search {
        summary: String,
        detail: String,
    },
    Question {
        summary: String,
        detail: String,
    },
    Approval {
        allowed: bool,
        reason: Option<&'a str>,
    },
    Retry {
        summary: &'a str,
        current: u64,
        max: u64,
    },
    Skill {
        name: &'a str,
        path: &'a str,
    },
    Compaction,
    Failure {
        message: &'a str,
    },
    Cancelled,
    Unsupported {
        summary: String,
    },
}

#[derive(Clone, PartialEq)]
pub(super) struct CommandDisplay {
    pub id_scope: String,
    pub summary: String,
    pub operations: Vec<CommandOperationDisplay>,
    pub approvals: Vec<CommandApprovalDisplay>,
}

#[derive(Clone, PartialEq)]
pub(super) struct CommandApprovalDisplay {
    pub allowed: bool,
    pub reason: Option<String>,
}

#[derive(Clone, PartialEq)]
pub(super) struct CommandOperationDisplay {
    pub runner: String,
    pub command: String,
    pub output: String,
    pub status: OperationStatus,
    pub omitted_bytes: usize,
    pub diff_files: SnapshotDiff,
    pub file_changes: Vec<FileChangeSummary>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum OperationStatus {
    Queued,
    Running {
        elapsed_ms: Option<u64>,
        remaining_ms: Option<u64>,
    },
    Finished {
        exit: Option<i32>,
    },
}

impl std::fmt::Display for OperationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OperationStatus::Queued => f.write_str("queued"),
            OperationStatus::Running {
                elapsed_ms: Some(elapsed),
                remaining_ms: Some(remaining),
            } => write!(
                f,
                "{} elapsed · {} until detach",
                format_duration(*elapsed),
                format_duration(*remaining)
            ),
            OperationStatus::Running { .. } => f.write_str("running"),
            OperationStatus::Finished { exit: Some(code) } => write!(f, "exit {code}"),
            OperationStatus::Finished { exit: None } => f.write_str("finished"),
        }
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct FileChangeSummary {
    pub path: String,
    pub operation: String,
    pub added: usize,
    pub deleted: usize,
}

pub(super) fn turn_keys(state: &ThreadState) -> Vec<TurnKey> {
    state
        .events()
        .iter()
        .filter(|event| is_turn_boundary(&event.data))
        .map(|event| TurnKey(event.sequence))
        .collect()
}

pub(super) fn raw_keys(state: &ThreadState) -> Vec<RawKey> {
    let mut keys = state
        .events()
        .iter()
        .map(|event| RawKey::Event(event.sequence))
        .collect::<Vec<_>>();
    if let Some(turn) = state.active_turn() {
        keys.extend(turn.items().iter().map(|item| RawKey::Active(item.id())));
    }
    keys
}

pub(super) fn raw_item(state: &ThreadState, key: &RawKey) -> Option<String> {
    match key {
        RawKey::Event(sequence) => event(state, *sequence).map(pretty),
        RawKey::Active(id) => state
            .active_turn()?
            .items()
            .iter()
            .find(|item| item.id() == *id)
            .map(pretty),
    }
}

pub(super) fn turn<'a>(state: &'a ThreadState, key: TurnKey) -> Option<TurnRef<'a>> {
    let start = event_index(state, key.sequence())?;
    let (prompt, prompt_sequence) = match &state.events()[start].data {
        ThreadEventData::UserMessage(message) => (
            message.content.as_str(),
            Some(state.events()[start].sequence),
        ),
        ThreadEventData::Compaction(_) => ("Compacted history", None),
        _ => return None,
    };
    let end = state.events()[start + 1..]
        .iter()
        .position(|event| is_turn_boundary(&event.data))
        .map_or(state.events().len(), |offset| start + 1 + offset);
    Some(TurnRef {
        state,
        start,
        end,
        prompt,
        prompt_sequence,
    })
}

pub(super) fn turn_key_for_event(state: &ThreadState, sequence: EventSequence) -> Option<TurnKey> {
    let index = event_index(state, sequence)?;
    state.events()[..=index]
        .iter()
        .rev()
        .find(|event| is_turn_boundary(&event.data))
        .map(|event| TurnKey(event.sequence))
}

impl<'a> TurnRef<'a> {
    pub fn prompt(&self) -> &'a str {
        self.prompt
    }

    pub fn prompt_sequence(&self) -> Option<EventSequence> {
        self.prompt_sequence
    }

    pub fn answer(&self) -> Option<(Option<EventSequence>, &'a str)> {
        if self.is_last()
            && let Some(active) = self.state.active_turn()
            && let Some(content) = active.items().iter().find_map(|item| match item.data() {
                ActiveItemData::Assistant {
                    content,
                    phase: AssistantMessagePhase::FinalAnswer,
                } => Some(content.as_str()),
                _ => None,
            })
        {
            return Some((None, content));
        }
        if let Some(answer) = self
            .events()
            .iter()
            .rev()
            .find_map(|event| match &event.data {
                ThreadEventData::AssistantMessage(message)
                    if message.phase == AssistantMessagePhase::FinalAnswer =>
                {
                    Some((Some(event.sequence), message.content.as_str()))
                }
                _ => None,
            })
        {
            return Some(answer);
        }
        None
    }

    pub fn outcome(&self) -> Option<&'a atra_protocol::TurnOutcome> {
        self.events()
            .iter()
            .rev()
            .find_map(|event| match &event.data {
                ThreadEventData::TurnOutcome(outcome) => Some(outcome),
                _ => None,
            })
    }

    pub fn activity_keys(&self) -> Vec<ActivityKey> {
        let mut activities = Vec::new();
        let mut pending_tools = HashMap::<String, usize>::new();
        let mut pending_tool_indices = Vec::new();

        for event in self.events() {
            match &event.data {
                ThreadEventData::AssistantMessage(message)
                    if message.phase == AssistantMessagePhase::Commentary =>
                {
                    activities.push(ActivityKey::Event(event.sequence));
                    append_todo_key(&mut activities, event.sequence, &message.todos);
                }
                ThreadEventData::AssistantMessage(message) => {
                    append_todo_key(&mut activities, event.sequence, &message.todos);
                }
                ThreadEventData::ToolCall(call) => {
                    let index = activities.len();
                    activities.push(ActivityKey::Tool {
                        call: event.sequence,
                        identity: tool_call_identity(call).map(str::to_owned),
                    });
                    pending_tool_indices.push(index);
                    if let Some(identity) = tool_call_identity(call) {
                        pending_tools.insert(identity.to_owned(), index);
                    }
                }
                ThreadEventData::ToolResult(result) => {
                    if let Some(index) = tool_result_identity(result)
                        .and_then(|identity| pending_tools.remove(identity))
                    {
                        pending_tool_indices.retain(|pending| *pending != index);
                    } else {
                        activities.push(ActivityKey::Event(event.sequence));
                    }
                }
                ThreadEventData::Reasoning(_)
                | ThreadEventData::WebSearch(_)
                | ThreadEventData::SkillInvocation(_)
                | ThreadEventData::Compaction(_)
                | ThreadEventData::Retry(_) => {
                    activities.push(ActivityKey::Event(event.sequence));
                }
                ThreadEventData::ApprovalDecision(_) => {
                    if pending_tool_indices.is_empty() {
                        activities.push(ActivityKey::Event(event.sequence));
                    }
                }
                ThreadEventData::TurnOutcome(_) => {}
                ThreadEventData::ThreadContext(_)
                | ThreadEventData::WorkspaceInstructions(_)
                | ThreadEventData::Skills(_)
                | ThreadEventData::Runners(_)
                | ThreadEventData::UserMessage(_)
                | ThreadEventData::ModelRequest(_)
                | ThreadEventData::TokenUsage(_)
                | ThreadEventData::RateLimits(_) => {}
            }
        }

        if self.is_last()
            && let Some(active) = self.state.active_turn()
        {
            for item in active.items().iter().filter(|item| {
                !matches!(
                    item.data(),
                    ActiveItemData::Assistant {
                        phase: AssistantMessagePhase::FinalAnswer,
                        ..
                    }
                )
            }) {
                if let ActiveItemData::RunnerTool { call_id, .. } = item.data()
                    && activities
                        .iter()
                        .any(|key| activity_identity(key).as_deref() == Some(call_id))
                {
                    continue;
                }
                activities.push(match item.data() {
                    ActiveItemData::ToolCall {
                        item_id, call_id, ..
                    } => call_id.clone().or_else(|| Some(item_id.clone())).map_or(
                        ActivityKey::Active(item.id()),
                        |identity| ActivityKey::StableActive {
                            id: item.id(),
                            identity,
                        },
                    ),
                    ActiveItemData::WebSearch { item_id, .. } => ActivityKey::StableActive {
                        id: item.id(),
                        identity: item_id.clone(),
                    },
                    _ => ActivityKey::Active(item.id()),
                });
            }
        }

        activities
    }

    pub fn is_active(&self) -> bool {
        self.is_last() && self.state.active_turn().is_some()
    }

    pub fn can_have_active(&self) -> bool {
        self.is_last()
    }

    fn events(&self) -> &'a [ThreadEvent] {
        &self.state.events()[self.start..self.end]
    }

    fn is_last(&self) -> bool {
        self.end == self.state.events().len()
    }
}

pub(super) fn activity<'a>(
    state: &'a ThreadState,
    key: &ActivityKey,
    finalized: &FinalizedActivities,
) -> Option<ActivityDisplay<'a>> {
    let resolved = resolve_activity_key(state, key, finalized);
    let key = &resolved;
    match key {
        ActivityKey::Event(sequence) => {
            let event = event(state, *sequence)?;
            match &event.data {
                ThreadEventData::AssistantMessage(message)
                    if message.phase == AssistantMessagePhase::Commentary =>
                {
                    Some(ActivityDisplay::Commentary {
                        markdown: &message.content,
                    })
                }
                ThreadEventData::Reasoning(reasoning) => Some(ActivityDisplay::Reasoning {
                    summary: Cow::Borrowed(&reasoning.summary),
                }),
                ThreadEventData::WebSearch(search) => {
                    let (summary, detail) = search_display(&search.item);
                    Some(ActivityDisplay::Search { summary, detail })
                }
                ThreadEventData::SkillInvocation(skill) => Some(ActivityDisplay::Skill {
                    name: &skill.name,
                    path: &skill.path,
                }),
                ThreadEventData::Compaction(_) => Some(ActivityDisplay::Compaction),
                ThreadEventData::ApprovalDecision(decision) => Some(ActivityDisplay::Approval {
                    allowed: decision.allowed,
                    reason: decision.reason.as_deref(),
                }),
                ThreadEventData::Retry(retry) => Some(ActivityDisplay::Retry {
                    summary: &retry.summary,
                    current: retry.current,
                    max: retry.max,
                }),
                ThreadEventData::TurnOutcome(atra_protocol::TurnOutcome::Failed { message }) => {
                    Some(ActivityDisplay::Failure { message })
                }
                ThreadEventData::TurnOutcome(atra_protocol::TurnOutcome::Cancelled) => {
                    Some(ActivityDisplay::Cancelled)
                }
                ThreadEventData::ToolResult(_) => Some(ActivityDisplay::Unsupported {
                    summary: "Unmatched tool result".to_owned(),
                }),
                _ => None,
            }
        }
        ActivityKey::Tool { call, identity } => {
            let call_sequence = *call;
            let call = match &event(state, call_sequence)?.data {
                ThreadEventData::ToolCall(call) => call,
                _ => return None,
            };
            let details = tool_activity_state(state, call_sequence);
            let result = details
                .result
                .and_then(|sequence| event(state, sequence))
                .and_then(|event| match &event.data {
                    ThreadEventData::ToolResult(result) => Some(result),
                    _ => None,
                });
            let name = tool_call_name(call);
            match canonical_tool_name(name) {
                "command" => Some(ActivityDisplay::Command(command_display(
                    state,
                    call,
                    result,
                    identity.as_deref(),
                    &details.approvals,
                ))),
                "question" => Some(question_display(call, result)),
                _ => Some(ActivityDisplay::Unsupported {
                    summary: tool_summary(call),
                }),
            }
        }
        ActivityKey::Todo { source } => {
            let message = match &event(state, *source)?.data {
                ThreadEventData::AssistantMessage(message) => message,
                _ => return None,
            };
            (!message.todos.is_empty()).then_some(ActivityDisplay::Todo {
                items: &message.todos,
            })
        }
        ActivityKey::Active(id) => {
            let item = state
                .active_turn()?
                .items()
                .iter()
                .find(|item| item.id() == *id)?;
            active_activity(state, item)
        }
        ActivityKey::StableActive { id, .. } => {
            let item = state
                .active_turn()
                .and_then(|turn| turn.items().iter().find(|item| item.id() == *id))?;
            active_activity(state, item)
        }
    }
}

pub(super) fn activity_can_receive_active_updates(
    state: &ThreadState,
    key: &ActivityKey,
    finalized: &FinalizedActivities,
) -> bool {
    match resolve_activity_key(state, key, finalized) {
        ActivityKey::Active(_) | ActivityKey::StableActive { .. } => true,
        ActivityKey::Tool { call, .. } => tool_activity_state(state, call).result.is_none(),
        ActivityKey::Event(_) | ActivityKey::Todo { .. } => false,
    }
}

fn active_activity<'a>(
    state: &'a ThreadState,
    item: &'a ActiveItem,
) -> Option<ActivityDisplay<'a>> {
    match item.data() {
        ActiveItemData::Reasoning { content } => Some(ActivityDisplay::Reasoning {
            summary: Cow::Borrowed(content),
        }),
        ActiveItemData::ToolCall {
            name,
            input,
            call_id,
            item_id,
        } => match canonical_tool_name(name) {
            "command" => {
                let call = ToolCallEvent::Custom {
                    item_id: Some(item_id.clone()),
                    name: name.clone(),
                    input: input.clone(),
                    call_id: call_id.clone().unwrap_or_else(|| item_id.clone()),
                };
                Some(ActivityDisplay::Command(command_display(
                    state,
                    &call,
                    None,
                    call_id.as_deref().or(Some(item_id)),
                    &[],
                )))
            }
            "question" => Some(active_question_display(input)),
            _ => Some(ActivityDisplay::Unsupported {
                summary: meaningful_text(input)
                    .unwrap_or_else(|| canonical_tool_name(name).to_owned()),
            }),
        },
        ActiveItemData::WebSearch { action, .. } => {
            let (summary, detail) = action
                .as_ref()
                .map(search_display)
                .unwrap_or_else(|| ("Searching…".to_owned(), String::new()));
            Some(ActivityDisplay::Search { summary, detail })
        }
        ActiveItemData::RunnerTool { runner, update, .. } => {
            Some(orphan_runner_display(item.id().0, runner, update))
        }
        ActiveItemData::Assistant { content, .. } => {
            Some(ActivityDisplay::Commentary { markdown: content })
        }
    }
}

fn canonical_tool_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

fn tool_call_name(call: &ToolCallEvent) -> &str {
    match call {
        ToolCallEvent::Custom { name, .. } | ToolCallEvent::Function { name, .. } => name,
    }
}

fn tool_call_input(call: &ToolCallEvent) -> String {
    match call {
        ToolCallEvent::Custom { input, .. } => input.clone(),
        ToolCallEvent::Function { arguments, .. } => arguments
            .get("input")
            .or_else(|| arguments.get("command"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_default(),
    }
}

fn tool_summary(call: &ToolCallEvent) -> String {
    let name = canonical_tool_name(tool_call_name(call));
    let input = tool_call_input(call);
    meaningful_text(&input)
        .map(|text| format!("{name}: {text}"))
        .unwrap_or_else(|| name.to_owned())
}

fn meaningful_text(value: &str) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn search_display(item: &Value) -> (String, String) {
    let query = item
        .get("query")
        .or_else(|| item.pointer("/action/query"))
        .or_else(|| item.get("url"))
        .and_then(Value::as_str);
    let action = item
        .get("type")
        .or_else(|| item.get("action"))
        .and_then(Value::as_str);
    let summary = match (action, query) {
        (Some(action), Some(query)) => format!("{action}: {query}"),
        (None, Some(query)) => query.to_owned(),
        (Some(action), None) => action.replace('_', " "),
        (None, None) => "Searching…".to_owned(),
    };
    let detail = item
        .get("results")
        .or_else(|| item.get("result"))
        .map(search_results_detail)
        .unwrap_or_default();
    (summary, detail)
}

fn search_results_detail(value: &Value) -> String {
    let results = value.as_array().map(Vec::as_slice).unwrap_or(&[]);
    results
        .iter()
        .filter_map(|result| {
            let title = result
                .get("title")
                .or_else(|| result.get("name"))
                .and_then(Value::as_str);
            let url = result.get("url").and_then(Value::as_str);
            let snippet = result
                .get("snippet")
                .or_else(|| result.get("text"))
                .and_then(Value::as_str);
            if title.is_none() && url.is_none() && snippet.is_none() {
                return None;
            }
            Some(
                [title, url, snippet]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn question_display(
    call: &ToolCallEvent,
    result: Option<&atra_protocol::ToolResultEvent>,
) -> ActivityDisplay<'static> {
    let arguments = match call {
        ToolCallEvent::Custom { input, .. } => serde_json::from_str(input).unwrap_or(Value::Null),
        ToolCallEvent::Function { arguments, .. } => arguments.clone(),
    };
    let summary = arguments
        .pointer("/questions/0/question")
        .and_then(Value::as_str)
        .unwrap_or("A question was asked")
        .to_owned();
    let mut detail = question_detail(&arguments);
    if let Some(result) = result {
        let value = tool_result_value(result);
        if !detail.is_empty() {
            detail.push_str("\n\n");
        }
        detail.push_str("Answer\n");
        detail.push_str(&question_answer_detail(value));
    }
    ActivityDisplay::Question { summary, detail }
}

fn active_question_display(input: &str) -> ActivityDisplay<'static> {
    let arguments = serde_json::from_str(input).unwrap_or(Value::Null);
    let summary = arguments
        .pointer("/questions/0/question")
        .and_then(Value::as_str)
        .unwrap_or("Preparing a question…")
        .to_owned();
    ActivityDisplay::Question {
        summary,
        detail: question_detail(&arguments),
    }
}

fn question_detail(arguments: &Value) -> String {
    arguments
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, question)| {
            let prompt = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| option.get("label").and_then(Value::as_str))
                .map(|label| format!("  • {label}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{}. {prompt}\n{options}", index + 1)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn question_answer_detail(value: &Value) -> String {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|answer| {
            let selected = answer
                .get("selected_option")
                .and_then(Value::as_str)
                .unwrap_or("No listed option");
            let note = answer
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if note.is_empty() {
                selected.to_owned()
            } else {
                format!("{selected} — {note}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_value(result: &atra_protocol::ToolResultEvent) -> &Value {
    match result {
        atra_protocol::ToolResultEvent::Custom { result, .. }
        | atra_protocol::ToolResultEvent::Function { result, .. } => result,
    }
}

fn command_display(
    state: &ThreadState,
    call: &ToolCallEvent,
    result: Option<&atra_protocol::ToolResultEvent>,
    identity: Option<&str>,
    approvals: &[EventSequence],
) -> CommandDisplay {
    let mut operations = command_operations(call)
        .into_iter()
        .map(|operation| CommandOperationDisplay {
            runner: operation.runner().to_owned(),
            command: operation.command().to_owned(),
            output: String::new(),
            status: OperationStatus::Queued,
            omitted_bytes: 0,
            diff_files: SnapshotDiff::default(),
            file_changes: Vec::new(),
        })
        .collect::<Vec<_>>();

    if let Some(result) = result {
        for artifact in tool_result_artifacts(result) {
            apply_command_artifact(artifact, &mut operations);
        }
    }

    if let Some(identity) = identity
        && let Some(turn) = state.active_turn()
    {
        for item in turn.items() {
            let ActiveItemData::RunnerTool {
                call_id,
                operation_index,
                runner,
                update,
            } = item.data()
            else {
                continue;
            };
            if call_id != identity {
                continue;
            }
            let index = operation_index.saturating_sub(1);
            if let Some(operation) = operations.get_mut(index) {
                operation.runner.clone_from(runner);
                apply_runner_update(update, operation);
            }
        }
    }

    let summary = operations
        .first()
        .map(|operation| {
            let command =
                meaningful_text(&operation.command).unwrap_or_else(|| "command".to_owned());
            let more = operations.len().saturating_sub(1);
            if more == 0 {
                format!("{command} — {}", operation.runner)
            } else {
                format!("{command} — {} · +{more}", operation.runner)
            }
        })
        .unwrap_or_else(|| "Invalid command input".to_owned());
    let approvals = approvals
        .iter()
        .filter_map(|sequence| match &event(state, *sequence)?.data {
            ThreadEventData::ApprovalDecision(decision) => Some(CommandApprovalDisplay {
                allowed: decision.allowed,
                reason: decision.reason.clone(),
            }),
            _ => None,
        })
        .collect();
    CommandDisplay {
        id_scope: tool_call_identity(call).unwrap_or("command").to_owned(),
        summary,
        operations,
        approvals,
    }
}

fn command_operations(call: &ToolCallEvent) -> Vec<atra_protocol::RunnerCommand> {
    match call {
        ToolCallEvent::Custom { input, .. } => atra_protocol::parse_command_input(input)
            .unwrap_or_else(|_| {
                vec![atra_protocol::RunnerCommand::from_parts(
                    "unknown".to_owned(),
                    input.clone(),
                )]
            }),
        ToolCallEvent::Function { arguments, .. } => {
            let runner = arguments.get("runner").and_then(Value::as_str);
            let command = arguments.get("command").and_then(Value::as_str);
            match (runner, command) {
                (Some(runner), Some(command)) => {
                    vec![atra_protocol::RunnerCommand::from_parts(
                        runner.to_owned(),
                        command.to_owned(),
                    )]
                }
                _ => vec![atra_protocol::RunnerCommand::from_parts(
                    "unknown".to_owned(),
                    tool_call_input(call),
                )],
            }
        }
    }
}

fn tool_result_artifacts(result: &atra_protocol::ToolResultEvent) -> &[ToolArtifact] {
    match result {
        atra_protocol::ToolResultEvent::Custom { artifacts, .. }
        | atra_protocol::ToolResultEvent::Function { artifacts, .. } => artifacts,
    }
}

fn apply_runner_update(update: &RunnerOperationUpdate, operation: &mut CommandOperationDisplay) {
    match update {
        RunnerOperationUpdate::CommandStarted { timer } => {
            operation.status = OperationStatus::Running {
                elapsed_ms: Some(timer.elapsed_ms),
                remaining_ms: Some(timer.remaining_ms),
            };
        }
        RunnerOperationUpdate::CommandOutput {
            content,
            omitted_bytes,
            timer,
        } => {
            operation.output = strip_ansi(content);
            operation.omitted_bytes = *omitted_bytes;
            operation.status = OperationStatus::Running {
                elapsed_ms: Some(timer.elapsed_ms),
                remaining_ms: Some(timer.remaining_ms),
            };
        }
        RunnerOperationUpdate::Completed { artifact } => {
            apply_operation_artifact(artifact, operation);
        }
    }
}

fn format_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    if seconds >= 60 * 60 {
        format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn apply_command_artifact(artifact: &ToolArtifact, operations: &mut [CommandOperationDisplay]) {
    match artifact {
        ToolArtifact::RunnerOperation(runner) => {
            let index = runner.operation.saturating_sub(1);
            if let Some(operation) = operations.get_mut(index) {
                apply_operation_artifact(artifact, operation);
            }
        }
        artifact if operations.len() == 1 => {
            apply_operation_artifact(artifact, &mut operations[0]);
        }
        _ => {}
    }
}

fn apply_operation_artifact(artifact: &ToolArtifact, operation: &mut CommandOperationDisplay) {
    match artifact {
        ToolArtifact::CommandExecution(CommandExecutionArtifact::Started { runner }) => {
            operation.runner.clone_from(runner);
            operation.status = OperationStatus::Running {
                elapsed_ms: None,
                remaining_ms: None,
            };
        }
        ToolArtifact::CommandExecution(CommandExecutionArtifact::Running {
            output,
            runner,
            ..
        }) => {
            operation.runner.clone_from(runner);
            operation.output = strip_ansi(output);
            operation.status = OperationStatus::Running {
                elapsed_ms: None,
                remaining_ms: None,
            };
        }
        ToolArtifact::CommandExecution(CommandExecutionArtifact::Finished {
            output,
            exit_code,
            runner,
            ..
        }) => {
            operation.runner.clone_from(runner);
            operation.output = strip_ansi(output);
            operation.status = OperationStatus::Finished { exit: *exit_code };
        }
        ToolArtifact::PatchOperations(patch) => {
            format_apply_patch(
                patch,
                &mut operation.output,
                &mut operation.file_changes,
                &mut operation.diff_files,
            );
        }
        ToolArtifact::RunnerOperation(runner) => {
            operation.runner.clone_from(&runner.runner);
            if let Some(result) = runner.result.as_str() {
                operation.output = strip_ansi(result);
            }
            let has_command_artifact = runner
                .artifacts
                .iter()
                .any(|artifact| matches!(artifact, ToolArtifact::CommandExecution(_)));
            for artifact in &runner.artifacts {
                apply_operation_artifact(artifact, operation);
            }
            if !has_command_artifact {
                operation.status = OperationStatus::Finished { exit: None };
            }
        }
    }
}

fn format_apply_patch(
    patch: &ApplyPatchResult,
    output: &mut String,
    file_changes: &mut Vec<FileChangeSummary>,
    diff_files: &mut SnapshotDiff,
) {
    match patch {
        ApplyPatchResult::ParseError { error } => {
            output.push_str("Patch parse error: ");
            output.push_str(error);
            output.push('\n');
        }
        ApplyPatchResult::Operations { results } => {
            for result in results {
                format_patch_operation(result, output, file_changes, diff_files);
            }
        }
    }
}

fn orphan_runner_display(
    identity: u64,
    runner: &str,
    update: &RunnerOperationUpdate,
) -> ActivityDisplay<'static> {
    let mut operation = CommandOperationDisplay {
        runner: runner.to_owned(),
        command: String::new(),
        output: String::new(),
        status: OperationStatus::Running {
            elapsed_ms: None,
            remaining_ms: None,
        },
        omitted_bytes: 0,
        diff_files: SnapshotDiff::default(),
        file_changes: Vec::new(),
    };
    apply_runner_update(update, &mut operation);
    ActivityDisplay::Command(CommandDisplay {
        id_scope: format!("runner-{identity}"),
        summary: format!("{} — {}", operation.status, operation.runner),
        operations: vec![operation],
        approvals: Vec::new(),
    })
}

pub(super) fn activity_summary(state: &ThreadState, keys: &[ActivityKey]) -> String {
    // While a turn is active, surface the current activity's summary for context.
    if let Some((summary, _)) = keys
        .iter()
        .filter_map(|key| activity_header(state, key))
        .find(|(_, active)| *active)
    {
        return summary;
    }
    // Completed turns collapse to a structural summary of what happened.
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for key in keys {
        let label = activity_type_label(state, key);
        match counts.iter_mut().find(|(existing, _)| *existing == label) {
            Some((_, count)) => *count += 1,
            None => counts.push((label, 1)),
        }
    }
    if counts.is_empty() {
        return "No activity".to_owned();
    }
    counts
        .iter()
        .map(|(label, count)| format_activity_count(label, *count))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn activity_type_label(state: &ThreadState, key: &ActivityKey) -> &'static str {
    if let ActivityKey::StableActive { id, identity } = key
        && state
            .active_turn()
            .is_none_or(|turn| turn.items().iter().all(|item| item.id() != *id))
        && let Some(key) = finalized_activity_key(state, identity)
    {
        return activity_type_label(state, &key);
    }
    match key {
        ActivityKey::Event(sequence) => {
            let Some(event) = event(state, *sequence) else {
                return "activity";
            };
            match &event.data {
                ThreadEventData::AssistantMessage(message)
                    if message.phase == AssistantMessagePhase::Commentary =>
                {
                    "update"
                }
                ThreadEventData::Reasoning(_) => "reasoning",
                ThreadEventData::WebSearch(_) => "search",
                ThreadEventData::SkillInvocation(_) => "skill",
                ThreadEventData::Compaction(_) => "compaction",
                ThreadEventData::Retry(_) => "retry",
                ThreadEventData::ApprovalDecision(_) => "approval",
                ThreadEventData::TurnOutcome(atra_protocol::TurnOutcome::Failed { .. }) => {
                    "failure"
                }
                ThreadEventData::TurnOutcome(atra_protocol::TurnOutcome::Cancelled) => "cancelled",
                _ => "activity",
            }
        }
        ActivityKey::Tool { call, .. } => {
            let Some(call) = event(state, *call).and_then(|event| match &event.data {
                ThreadEventData::ToolCall(call) => Some(call),
                _ => None,
            }) else {
                return "activity";
            };
            match canonical_tool_name(tool_call_name(call)) {
                "command" => "command",
                "question" => "question",
                _ => "tool",
            }
        }
        ActivityKey::Todo { .. } => "todo",
        ActivityKey::Active(id) | ActivityKey::StableActive { id, .. } => {
            let Some(item) = state
                .active_turn()
                .and_then(|turn| turn.items().iter().find(|item| item.id() == *id))
            else {
                return "activity";
            };
            match item.data() {
                ActiveItemData::Reasoning { .. } => "reasoning",
                ActiveItemData::ToolCall { name, .. } => match canonical_tool_name(name) {
                    "command" => "command",
                    "question" => "question",
                    _ => "tool",
                },
                ActiveItemData::WebSearch { .. } => "search",
                ActiveItemData::RunnerTool { .. } => "command",
                ActiveItemData::Assistant { .. } => "update",
            }
        }
    }
}

fn format_activity_count(label: &str, count: usize) -> String {
    let plural = match label {
        "command" => "commands",
        "search" => "searches",
        "todo" => "todos",
        "update" => "updates",
        "question" => "questions",
        "approval" => "approvals",
        "retry" => "retries",
        "skill" => "skills",
        "boundary" => "boundaries",
        "failure" => "failures",
        _ => label,
    };
    if count == 1 {
        format!("1 {label}")
    } else {
        format!("{count} {plural}")
    }
}

fn activity_header(state: &ThreadState, key: &ActivityKey) -> Option<(String, bool)> {
    if let ActivityKey::StableActive { id, identity } = key
        && state
            .active_turn()
            .is_none_or(|turn| turn.items().iter().all(|item| item.id() != *id))
        && let Some(key) = finalized_activity_key(state, identity)
    {
        return activity_header(state, &key);
    }
    match key {
        ActivityKey::Event(sequence) => match &event(state, *sequence)?.data {
            ThreadEventData::AssistantMessage(message)
                if message.phase == AssistantMessagePhase::Commentary =>
            {
                Some((
                    meaningful_text(&message.content).unwrap_or_else(|| "Update".to_owned()),
                    false,
                ))
            }
            ThreadEventData::Reasoning(reasoning) => Some((
                meaningful_text(&reasoning.summary).unwrap_or_else(|| "Thinking…".to_owned()),
                false,
            )),
            ThreadEventData::WebSearch(search) => {
                let (summary, _) = search_display(&search.item);
                Some((summary, false))
            }
            ThreadEventData::SkillInvocation(skill) => {
                Some((format!("{} — {}", skill.name, skill.path), false))
            }
            ThreadEventData::Compaction(_) => {
                Some(("Compacting conversation history".to_owned(), false))
            }
            ThreadEventData::Retry(retry) => Some((
                format!(
                    "{} · attempt {}/{}",
                    retry.summary, retry.current, retry.max
                ),
                false,
            )),
            ThreadEventData::ApprovalDecision(decision) => Some((
                if decision.allowed {
                    "Approved".to_owned()
                } else {
                    decision
                        .reason
                        .as_deref()
                        .map(|reason| format!("Denied — {reason}"))
                        .unwrap_or_else(|| "Denied".to_owned())
                },
                false,
            )),
            _ => None,
        },
        ActivityKey::Tool { call, identity, .. } => {
            let call = match &event(state, *call)?.data {
                ThreadEventData::ToolCall(call) => call,
                _ => return None,
            };
            let active = identity.as_deref().is_some_and(|identity| {
                state.active_turn().is_some_and(|turn| {
                    turn.items().iter().any(|item| {
                        matches!(
                            item.data(),
                            ActiveItemData::RunnerTool { call_id, .. } if call_id == identity
                        )
                    })
                })
            });
            Some((
                if canonical_tool_name(tool_call_name(call)) == "command" {
                    command_input_summary(&tool_call_input(call))
                } else if canonical_tool_name(tool_call_name(call)) == "question" {
                    match question_display(call, None) {
                        ActivityDisplay::Question { summary, .. } => summary,
                        _ => unreachable!(),
                    }
                } else {
                    tool_summary(call)
                },
                active,
            ))
        }
        ActivityKey::Todo { source } => {
            let message = match &event(state, *source)?.data {
                ThreadEventData::AssistantMessage(message) => message,
                _ => return None,
            };
            let completed = message
                .todos
                .iter()
                .filter(|item| matches!(item.status, TodoStatus::Completed))
                .count();
            let current = message
                .todos
                .iter()
                .find(|item| matches!(item.status, TodoStatus::InProgress))
                .or_else(|| {
                    message
                        .todos
                        .iter()
                        .find(|item| matches!(item.status, TodoStatus::Pending))
                })
                .map(|item| item.step.as_str())
                .unwrap_or("Plan complete");
            Some((
                format!("{current} · {completed}/{}", message.todos.len()),
                false,
            ))
        }
        ActivityKey::Active(id) | ActivityKey::StableActive { id, .. } => {
            let item = state
                .active_turn()?
                .items()
                .iter()
                .find(|item| item.id() == *id)?;
            match item.data() {
                ActiveItemData::Reasoning { content } => Some((
                    meaningful_text(content).unwrap_or_else(|| "Thinking…".to_owned()),
                    true,
                )),
                ActiveItemData::ToolCall { name, input, .. }
                    if canonical_tool_name(name) == "command" =>
                {
                    Some((command_input_summary(input), true))
                }
                ActiveItemData::ToolCall { name, input, .. }
                    if canonical_tool_name(name) == "question" =>
                {
                    let summary = match active_question_display(input) {
                        ActivityDisplay::Question { summary, .. } => summary,
                        _ => unreachable!(),
                    };
                    Some((summary, true))
                }
                ActiveItemData::ToolCall { name, input, .. } => Some((
                    meaningful_text(input).unwrap_or_else(|| canonical_tool_name(name).to_owned()),
                    true,
                )),
                ActiveItemData::WebSearch { action, .. } => Some((
                    action
                        .as_ref()
                        .map(search_display)
                        .map(|(summary, _)| summary)
                        .unwrap_or_else(|| "Searching…".to_owned()),
                    true,
                )),
                ActiveItemData::RunnerTool { runner, .. } => {
                    Some((format!("Running on {runner}"), true))
                }
                ActiveItemData::Assistant { content, .. } => Some((
                    meaningful_text(content).unwrap_or_else(|| "Writing…".to_owned()),
                    true,
                )),
            }
        }
    }
}

fn command_input_summary(input: &str) -> String {
    let Ok(operations) = atra_protocol::parse_command_input(input) else {
        return meaningful_text(input).unwrap_or_else(|| "Preparing command…".to_owned());
    };
    let Some(operation) = operations.first() else {
        return "Preparing command…".to_owned();
    };
    let command =
        meaningful_text(operation.command()).unwrap_or_else(|| "Preparing command…".to_owned());
    let more = operations.len().saturating_sub(1);
    if more == 0 {
        format!("{command} — {}", operation.runner())
    } else {
        format!("{command} — {} · +{more}", operation.runner())
    }
}

fn event(state: &ThreadState, sequence: EventSequence) -> Option<&ThreadEvent> {
    event_index(state, sequence).map(|index| &state.events()[index])
}

fn event_index(state: &ThreadState, sequence: EventSequence) -> Option<usize> {
    state
        .events()
        .binary_search_by_key(&sequence, |event| event.sequence)
        .ok()
}

fn append_todo_key(
    keys: &mut Vec<ActivityKey>,
    source: EventSequence,
    todos: &[atra_protocol::TodoItem],
) {
    if !todos.is_empty() {
        keys.push(ActivityKey::Todo { source });
    }
}

fn tool_call_identity(call: &ToolCallEvent) -> Option<&str> {
    match call {
        ToolCallEvent::Custom { call_id, .. } => Some(call_id),
        ToolCallEvent::Function { call_id, .. } => Some(call_id),
    }
}

fn tool_result_identity(result: &ToolResultEvent) -> Option<&str> {
    match result {
        ToolResultEvent::Custom { call_id, .. } => Some(call_id),
        ToolResultEvent::Function { call_id, .. } => Some(call_id),
    }
}

#[derive(Default)]
struct ToolActivityState {
    result: Option<EventSequence>,
    approvals: Vec<EventSequence>,
}

fn tool_activity_state(state: &ThreadState, target: EventSequence) -> ToolActivityState {
    let Some(turn) = turn_key_for_event(state, target).and_then(|key| turn(state, key)) else {
        return ToolActivityState::default();
    };
    let events = turn.events();
    let Ok(target_index) = events.binary_search_by_key(&target, |event| event.sequence) else {
        return ToolActivityState::default();
    };
    let ThreadEventData::ToolCall(target_call) = &events[target_index].data else {
        return ToolActivityState::default();
    };
    let target_identity = tool_call_identity(target_call);
    let mut state = ToolActivityState::default();
    let mut later_pending = Vec::new();

    for event in &events[target_index + 1..] {
        match &event.data {
            ThreadEventData::ToolCall(call) => {
                later_pending.push(tool_call_identity(call));
            }
            ThreadEventData::ToolResult(result) => {
                if let Some(identity) = tool_result_identity(result) {
                    if let Some(index) = later_pending
                        .iter()
                        .rposition(|pending| *pending == Some(identity))
                    {
                        later_pending.remove(index);
                    } else if target_identity == Some(identity) {
                        state.result = Some(event.sequence);
                        break;
                    }
                }
            }
            ThreadEventData::ApprovalDecision(_) => {
                if later_pending.is_empty() {
                    state.approvals.push(event.sequence);
                }
            }
            _ => {}
        }
    }

    state
}

pub(super) fn activity_identity(key: &ActivityKey) -> Option<String> {
    match key {
        ActivityKey::Tool { identity, .. } => identity.clone(),
        ActivityKey::StableActive { identity, .. } => Some(identity.clone()),
        _ => None,
    }
}

pub(super) fn resolve_activity_key(
    state: &ThreadState,
    key: &ActivityKey,
    finalized: &FinalizedActivities,
) -> ActivityKey {
    match key {
        ActivityKey::StableActive { id, identity } => {
            let is_active = state
                .active_turn()
                .is_some_and(|turn| turn.items().iter().any(|item| item.id() == *id));
            if is_active {
                key.clone()
            } else {
                finalized_activity_key(state, identity).unwrap_or_else(|| key.clone())
            }
        }
        ActivityKey::Active(id)
            if state
                .active_turn()
                .is_none_or(|turn| turn.items().iter().all(|item| item.id() != *id)) =>
        {
            finalized
                .by_active_id
                .get(id)
                .map(|sequence| ActivityKey::Event(*sequence))
                .unwrap_or_else(|| key.clone())
        }
        _ => key.clone(),
    }
}

fn finalized_activity_key(state: &ThreadState, identity: &str) -> Option<ActivityKey> {
    turn_keys(state)
        .into_iter()
        .filter_map(|key| turn(state, key))
        .flat_map(|turn| turn.activity_keys())
        .find(|key| match key {
            ActivityKey::Tool { call, .. } => event(state, *call)
                .and_then(|event| match &event.data {
                    ThreadEventData::ToolCall(call) => Some(call),
                    _ => None,
                })
                .is_some_and(|call| tool_call_matches_identity(call, identity)),
            ActivityKey::Event(sequence) => event(state, *sequence)
                .and_then(|event| match &event.data {
                    ThreadEventData::WebSearch(search) => Some(&search.item),
                    _ => None,
                })
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
                .is_some_and(|id| id == identity),
            _ => false,
        })
}

fn tool_call_matches_identity(call: &ToolCallEvent, identity: &str) -> bool {
    match call {
        ToolCallEvent::Custom {
            item_id, call_id, ..
        } => call_id == identity || item_id.as_deref() == Some(identity),
        ToolCallEvent::Function { call_id, .. } => call_id == identity,
    }
}

fn format_patch_operation(
    result: &PatchOperationResult,
    output: &mut String,
    file_changes: &mut Vec<FileChangeSummary>,
    diff_files: &mut SnapshotDiff,
) {
    let (operation, path, outcome) = match result {
        PatchOperationResult::Added { path, outcome } => ("added", path, outcome),
        PatchOperationResult::Deleted { path, outcome } => ("deleted", path, outcome),
        PatchOperationResult::Updated { path, outcome } => ("updated", path, outcome),
        PatchOperationResult::Moved {
            from: _,
            to,
            outcome,
        } => ("moved", to, outcome),
    };
    match outcome {
        PatchOperationOutcome::Applied { diff: Ok(diff) } => {
            diff_files.push(adapt_patch_diff(operation, diff));
            let (added, deleted) = diff_stat(diff);
            file_changes.push(FileChangeSummary {
                path: diff
                    .new_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| path.display().to_string()),
                operation: operation.to_owned(),
                added,
                deleted,
            });
        }
        PatchOperationOutcome::Applied { diff: Err(error) }
        | PatchOperationOutcome::Failed { error } => {
            output.push_str("Patch failed: ");
            output.push_str(error);
            output.push('\n');
        }
    }
}

fn adapt_patch_diff(operation: &str, diff: &FileDiff) -> DiffViewFile {
    DiffViewFile {
        status: DiffViewStatus::Patch(operation.to_owned()),
        old_path: diff
            .old_path
            .as_ref()
            .map(|path| path.display().to_string()),
        new_path: diff
            .new_path
            .as_ref()
            .map(|path| path.display().to_string()),
        additions: diff_stat(diff).0 as u64,
        deletions: diff_stat(diff).1 as u64,
        kind: DiffViewKind::Text,
        mode_change: None,
        hunks: diff
            .hunks
            .iter()
            .map(|hunk| {
                let mut old_line = u32::try_from(hunk.old_start).unwrap_or(u32::MAX);
                let mut new_line = u32::try_from(hunk.new_start).unwrap_or(u32::MAX);
                DiffViewHunk {
                    header: format!(
                        "@@ -{},{} +{},{} @@",
                        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
                    ),
                    lines: hunk
                        .lines
                        .iter()
                        .map(|line| {
                            let (kind, old_number, new_number) = match line.kind {
                                DiffLineKind::Context => {
                                    let result =
                                        (DiffViewLineKind::Context, Some(old_line), Some(new_line));
                                    old_line += 1;
                                    new_line += 1;
                                    result
                                }
                                DiffLineKind::Added => {
                                    let result = (DiffViewLineKind::Addition, None, Some(new_line));
                                    new_line += 1;
                                    result
                                }
                                DiffLineKind::Removed => {
                                    let result = (DiffViewLineKind::Deletion, Some(old_line), None);
                                    old_line += 1;
                                    result
                                }
                            };
                            DiffViewLine {
                                kind,
                                content: line.text.clone(),
                                old_line: old_number,
                                new_line: new_number,
                                no_newline_at_eof: false,
                            }
                        })
                        .collect(),
                    truncated: false,
                }
            })
            .collect(),
        truncated: false,
        message: None,
    }
}

fn diff_stat(diff: &FileDiff) -> (usize, usize) {
    let mut added = 0;
    let mut deleted = 0;
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Added => added += 1,
                DiffLineKind::Removed => deleted += 1,
                DiffLineKind::Context => {}
            }
        }
    }
    (added, deleted)
}

#[cfg(test)]
fn format_file_diff(diff: &FileDiff, output: &mut String) {
    output.push_str("--- ");
    output.push_str(
        &diff
            .old_path
            .as_ref()
            .map(|path| format!("a/{}", path.display()))
            .unwrap_or_else(|| "/dev/null".to_owned()),
    );
    output.push_str("\n+++ ");
    output.push_str(
        &diff
            .new_path
            .as_ref()
            .map(|path| format!("b/{}", path.display()))
            .unwrap_or_else(|| "/dev/null".to_owned()),
    );
    output.push('\n');
    for hunk in &diff.hunks {
        output.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ));
        for line in &hunk.lines {
            output.push(match line.kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Added => '+',
                DiffLineKind::Removed => '-',
            });
            output.push_str(&line.text);
            output.push('\n');
        }
    }
}

fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        } else if !character.is_control() || matches!(character, '\n' | '\r' | '\t') {
            output.push(character);
        }
    }
    output
}

fn is_turn_boundary(data: &ThreadEventData) -> bool {
    matches!(
        data,
        ThreadEventData::UserMessage(_) | ThreadEventData::Compaction(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use atra_protocol::{
        ActiveItem, CommandTimerState, RunnerOperationArtifact, RunnerOperationUpdate,
        ThreadOperation, TurnPhase,
    };
    use serde_json::json;
    use std::path::PathBuf;

    fn event(sequence: i64, kind: &str, payload: Value) -> ThreadEvent {
        serde_json::from_value(json!({
            "sequence": sequence,
            "kind": kind,
            "payload": payload,
        }))
        .unwrap()
    }

    fn state(events: Vec<ThreadEvent>) -> ThreadState {
        serde_json::from_value(json!({
            "metadata": {
                "id": 1,
                "parent_thread_id": null,
                "display_name": "Thread",
                "provider": "fake",
                "model": "model",
                "reasoning_effort": "medium"
            },
            "events": events,
            "active_turn": null,
            "last_outcome": null,
            "checkpoints": [],
            "processes": []
        }))
        .unwrap()
    }

    #[test]
    fn flat_events_are_exposed_as_borrowed_turns() {
        let state = state(vec![
            event(1, "user_message", json!({"content": "first"})),
            event(
                2,
                "assistant_message",
                json!({"content": "working", "phase": "commentary"}),
            ),
            event(
                3,
                "assistant_message",
                json!({"content": "answer", "phase": "final_answer", "todos": [
                    {"step": "done", "status": "completed"}
                ]}),
            ),
            event(4, "user_message", json!({"content": "second"})),
        ]);

        assert_eq!(
            turn_keys(&state),
            vec![TurnKey(EventSequence(1)), TurnKey(EventSequence(4))]
        );
        let first = turn(&state, TurnKey(EventSequence(1))).unwrap();
        assert_eq!(first.prompt(), "first");
        assert_eq!(first.answer(), Some((Some(EventSequence(3)), "answer")));
        assert_eq!(
            first.activity_keys(),
            vec![
                ActivityKey::Event(EventSequence(2)),
                ActivityKey::Todo {
                    source: EventSequence(3),
                },
            ]
        );
    }

    #[test]
    fn tool_results_pair_with_calls_without_storing_a_projection() {
        let state = state(vec![
            event(1, "user_message", json!({"content": "run"})),
            event(
                2,
                "tool_call",
                json!({"type": "function", "call_id": "call-1", "name": "shell", "arguments": {"cmd": "pwd"}}),
            ),
            event(
                3,
                "tool_result",
                json!({
                    "type": "function",
                    "call_id": "call-1",
                    "name": "shell",
                    "result": {"output": "/tmp"},
                    "artifacts": []
                }),
            ),
        ]);
        let turn = turn(&state, TurnKey(EventSequence(1))).unwrap();
        assert_eq!(
            turn.activity_keys(),
            vec![ActivityKey::Tool {
                call: EventSequence(2),
                identity: Some("call-1".to_owned()),
            }]
        );
    }

    #[test]
    fn active_assistant_streams_in_the_answer_position() {
        let mut state = state(vec![event(1, "user_message", json!({"content": "prompt"}))]);
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(7),
                ActiveItemData::Assistant {
                    content: "streaming".to_owned(),
                    phase: AssistantMessagePhase::FinalAnswer,
                },
            ),
        }
        .apply(&mut state)
        .unwrap();

        let turn = turn(&state, TurnKey(EventSequence(1))).unwrap();
        assert!(turn.activity_keys().is_empty());
        assert_eq!(turn.answer(), Some((None, "streaming")));
    }

    #[test]
    fn active_commentary_streams_inside_the_activity_group() {
        let mut state = state(vec![event(1, "user_message", json!({"content": "prompt"}))]);
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(7),
                ActiveItemData::Assistant {
                    content: "working".to_owned(),
                    phase: AssistantMessagePhase::Commentary,
                },
            ),
        }
        .apply(&mut state)
        .unwrap();

        let turn = turn(&state, TurnKey(EventSequence(1))).unwrap();
        assert_eq!(
            turn.activity_keys(),
            vec![ActivityKey::Active(ActiveItemId(7))]
        );
        assert_eq!(turn.answer(), None);
        assert!(matches!(
            activity(
                &state,
                &turn.activity_keys()[0],
                &FinalizedActivities::default()
            ),
            Some(ActivityDisplay::Commentary {
                markdown: "working"
            })
        ));
    }

    #[test]
    fn stable_command_selection_follows_item_id_to_call_id() {
        let mut state = state(vec![event(1, "user_message", json!({"content": "run"}))]);
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(7),
                ActiveItemData::ToolCall {
                    item_id: "item-1".to_owned(),
                    call_id: None,
                    name: "command".to_owned(),
                    input: r#"{"command":"*** Runner sandbox\necho hello"}"#.to_owned(),
                },
            ),
        }
        .apply(&mut state)
        .unwrap();
        let selected = ActivityKey::StableActive {
            id: ActiveItemId(7),
            identity: "item-1".to_owned(),
        };
        ThreadOperation::ActiveItemFinalized {
            active_id: ActiveItemId(7),
            event: event(
                2,
                "tool_call",
                json!({
                    "type": "custom",
                    "item_id": "item-1",
                    "name": "command",
                    "input": "{\"command\":\"*** Runner sandbox\\necho hello\"}",
                    "call_id": "call-1"
                }),
            ),
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(8),
                ActiveItemData::RunnerTool {
                    call_id: "call-1".to_owned(),
                    operation_index: 1,
                    runner: "sandbox".to_owned(),
                    update: RunnerOperationUpdate::CommandOutput {
                        content: "first\nsecond\n".to_owned(),
                        omitted_bytes: 0,
                        timer: CommandTimerState {
                            elapsed_ms: 10,
                            remaining_ms: 20,
                            paused: false,
                        },
                    },
                },
            ),
        }
        .apply(&mut state)
        .unwrap();

        let ActivityDisplay::Command(running) =
            activity(&state, &selected, &FinalizedActivities::default()).unwrap()
        else {
            panic!("expected command display");
        };
        assert!(matches!(
            running.operations[0].status,
            OperationStatus::Running { .. }
        ));
        assert_eq!(running.operations[0].output, "first\nsecond\n");

        ThreadOperation::ActiveRunnerOutputAppended {
            id: ActiveItemId(8),
            content: "third\n".to_owned(),
            omitted_bytes: 0,
            timer: CommandTimerState {
                elapsed_ms: 20,
                remaining_ms: 10,
                paused: false,
            },
        }
        .apply(&mut state)
        .unwrap();
        let ActivityDisplay::Command(streaming) =
            activity(&state, &selected, &FinalizedActivities::default()).unwrap()
        else {
            panic!("expected command display");
        };
        assert_eq!(streaming.operations[0].output, "first\nsecond\nthird\n");

        ThreadOperation::ActiveRunnerUpdated {
            id: ActiveItemId(8),
            update: RunnerOperationUpdate::Completed {
                artifact: ToolArtifact::RunnerOperation(RunnerOperationArtifact {
                    operation: 1,
                    runner: "sandbox".to_owned(),
                    label: "Command".to_owned(),
                    result: Value::String("first\nsecond\nthird\n".to_owned()),
                    artifacts: Vec::new(),
                }),
            },
        }
        .apply(&mut state)
        .unwrap();
        let ActivityDisplay::Command(completed) =
            activity(&state, &selected, &FinalizedActivities::default()).unwrap()
        else {
            panic!("expected command display");
        };
        assert_eq!(
            completed.operations[0].status,
            OperationStatus::Finished { exit: None }
        );
        assert_eq!(
            activity_identity(&resolve_activity_key(
                &state,
                &selected,
                &FinalizedActivities::default()
            ))
            .as_deref(),
            Some("call-1")
        );
        assert_eq!(activity_type_label(&state, &selected), "command");
    }

    #[test]
    fn stable_web_search_selection_follows_item_id_to_the_finalized_event() {
        let mut state = state(vec![event(1, "user_message", json!({"content": "search"}))]);
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(7),
                ActiveItemData::WebSearch {
                    item_id: "item-1".to_owned(),
                    action: None,
                },
            ),
        }
        .apply(&mut state)
        .unwrap();
        let selected = ActivityKey::StableActive {
            id: ActiveItemId(7),
            identity: "item-1".to_owned(),
        };

        // While the item is still active the selection resolves to the streaming item.
        assert!(matches!(
            activity(&state, &selected, &FinalizedActivities::default()),
            Some(ActivityDisplay::Search { .. })
        ));
        assert_eq!(activity_type_label(&state, &selected), "search");

        // Finalizing the item into a WebSearch event carrying the same id keeps the selection.
        ThreadOperation::ActiveItemFinalized {
            active_id: ActiveItemId(7),
            event: event(
                2,
                "web_search",
                json!({"item": {"id": "item-1", "type": "search", "query": "atra"}}),
            ),
        }
        .apply(&mut state)
        .unwrap();
        assert_eq!(
            resolve_activity_key(&state, &selected, &FinalizedActivities::default()),
            ActivityKey::Event(EventSequence(2))
        );
        let ActivityDisplay::Search { summary, .. } =
            activity(&state, &selected, &FinalizedActivities::default()).unwrap()
        else {
            panic!("expected search display");
        };
        assert_eq!(summary, "search: atra");
        assert_eq!(activity_type_label(&state, &selected), "search");
    }

    #[test]
    fn reasoning_selection_follows_active_item_to_the_finalized_event() {
        let mut state = state(vec![event(
            1,
            "user_message",
            json!({"content": "question"}),
        )]);
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(9),
                ActiveItemData::Reasoning {
                    content: "streaming…".to_owned(),
                },
            ),
        }
        .apply(&mut state)
        .unwrap();
        let selected = ActivityKey::Active(ActiveItemId(9));

        // While the item is still active the selection resolves to the streaming item.
        assert!(matches!(
            activity(&state, &selected, &FinalizedActivities::default()),
            Some(ActivityDisplay::Reasoning { .. })
        ));

        // Finalizing the item into a Reasoning event keeps the selection.
        ThreadOperation::ActiveItemFinalized {
            active_id: ActiveItemId(9),
            event: event(2, "reasoning", json!({"summary": "complete reasoning"})),
        }
        .apply(&mut state)
        .unwrap();
        let finalized = FinalizedActivities {
            by_active_id: HashMap::from([(ActiveItemId(9), EventSequence(2))]),
        };
        assert_eq!(
            resolve_activity_key(&state, &selected, &finalized),
            ActivityKey::Event(EventSequence(2))
        );
        let ActivityDisplay::Reasoning { summary } =
            activity(&state, &selected, &finalized).unwrap()
        else {
            panic!("expected reasoning display");
        };
        assert_eq!(summary, "complete reasoning");
        assert!(!activity_can_receive_active_updates(
            &state, &selected, &finalized
        ));
    }

    #[test]
    fn finalized_command_selection_reads_the_latest_tool_result() {
        let mut state = state(vec![
            event(1, "user_message", json!({"content": "run"})),
            event(
                2,
                "tool_call",
                json!({
                    "type": "function",
                    "name": "command",
                    "call_id": "call-1",
                    "arguments": {
                        "runner": "sandbox",
                        "command": "echo hello"
                    }
                }),
            ),
        ]);
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(8),
                ActiveItemData::RunnerTool {
                    call_id: "call-1".to_owned(),
                    operation_index: 1,
                    runner: "sandbox".to_owned(),
                    update: RunnerOperationUpdate::Completed {
                        artifact: ToolArtifact::RunnerOperation(RunnerOperationArtifact {
                            operation: 1,
                            runner: "sandbox".to_owned(),
                            label: "Command".to_owned(),
                            result: Value::String("streamed\n".to_owned()),
                            artifacts: Vec::new(),
                        }),
                    },
                },
            ),
        }
        .apply(&mut state)
        .unwrap();
        let selected = turn(&state, TurnKey(EventSequence(1)))
            .unwrap()
            .activity_keys()[0]
            .clone();
        assert!(activity_can_receive_active_updates(
            &state,
            &selected,
            &FinalizedActivities::default()
        ));

        ThreadOperation::ToolResultFinalized {
            event: event(
                3,
                "tool_result",
                json!({
                    "type": "function",
                    "name": "command",
                    "call_id": "call-1",
                    "result": "persisted\n",
                    "artifacts": [{
                        "kind": "runner_operation",
                        "data": {
                            "operation": 1,
                            "runner": "sandbox",
                            "label": "Command",
                            "result": "persisted\n",
                            "artifacts": []
                        }
                    }]
                }),
            ),
            runner_ids: vec![ActiveItemId(8)],
        }
        .apply(&mut state)
        .unwrap();

        let ActivityDisplay::Command(completed) =
            activity(&state, &selected, &FinalizedActivities::default()).unwrap()
        else {
            panic!("expected command display");
        };
        assert_eq!(
            completed.operations[0].status,
            OperationStatus::Finished { exit: None }
        );
        assert_eq!(completed.operations[0].output, "persisted\n");
        assert!(!activity_can_receive_active_updates(
            &state,
            &selected,
            &FinalizedActivities::default()
        ));
    }

    #[test]
    fn active_runner_output_is_grouped_under_its_tool_call() {
        let mut state = state(vec![
            event(1, "user_message", json!({"content": "run"})),
            event(
                2,
                "tool_call",
                json!({
                    "type": "function",
                    "name": "command",
                    "call_id": "call-1",
                    "arguments": {
                        "runner": "sandbox",
                        "command": "echo hello"
                    }
                }),
            ),
        ]);
        let selected = turn(&state, TurnKey(EventSequence(1)))
            .unwrap()
            .activity_keys()[0]
            .clone();
        let ActivityDisplay::Command(queued) =
            activity(&state, &selected, &FinalizedActivities::default()).unwrap()
        else {
            panic!("expected command display");
        };
        assert_eq!(queued.operations[0].runner, "sandbox");
        assert_eq!(queued.operations[0].command, "echo hello");
        assert_eq!(queued.operations[0].status, OperationStatus::Queued);

        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(8),
                ActiveItemData::RunnerTool {
                    call_id: "call-1".to_owned(),
                    operation_index: 1,
                    runner: "sandbox".to_owned(),
                    update: RunnerOperationUpdate::CommandOutput {
                        content: "hello\n".to_owned(),
                        omitted_bytes: 0,
                        timer: CommandTimerState {
                            elapsed_ms: 10,
                            remaining_ms: 0,
                            paused: false,
                        },
                    },
                },
            ),
        }
        .apply(&mut state)
        .unwrap();

        let turn = turn(&state, TurnKey(EventSequence(1))).unwrap();
        assert_eq!(turn.activity_keys().len(), 1);
        let display = activity(
            &state,
            &turn.activity_keys()[0],
            &FinalizedActivities::default(),
        )
        .unwrap();
        let ActivityDisplay::Command(display) = display else {
            panic!("expected command display");
        };
        assert!(display.operations[0].output.contains("hello"));
        assert_eq!(display.operations[0].runner, "sandbox");
        assert_eq!(display.operations[0].command, "echo hello");
        assert!(matches!(
            display.operations[0].status,
            OperationStatus::Running { .. }
        ));
    }

    #[test]
    fn reasoning_projection_exposes_only_summary_text() {
        let state = state(vec![
            event(1, "user_message", json!({"content": "think"})),
            event(
                2,
                "reasoning",
                json!({
                    "summary": "Public summary",
                    "opaque": {
                        "replay_key": "fixture/model/reasoning-v1",
                        "payload": {
                            "encrypted_content": "must-never-render",
                            "provider_metadata": {"secret": true}
                        }
                    }
                }),
            ),
        ]);
        let display = activity(
            &state,
            &ActivityKey::Event(EventSequence(2)),
            &FinalizedActivities::default(),
        )
        .unwrap();
        let ActivityDisplay::Reasoning { summary, .. } = display else {
            panic!("expected reasoning");
        };
        assert_eq!(summary, "Public summary");
        assert!(!summary.contains("must-never-render"));
    }

    #[test]
    fn patch_diff_is_rendered_as_unified_diff() {
        let diff = FileDiff {
            old_path: Some(PathBuf::from("src/main.rs")),
            new_path: Some(PathBuf::from("src/main.rs")),
            hunks: vec![atra_patch_types::DiffHunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![
                    atra_patch_types::DiffLine {
                        kind: DiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                        text: "old".to_owned(),
                    },
                    atra_patch_types::DiffLine {
                        kind: DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        text: "new".to_owned(),
                    },
                ],
            }],
        };
        let mut rendered = String::new();
        format_file_diff(&diff, &mut rendered);
        assert!(rendered.contains("--- a/src/main.rs"));
        assert!(rendered.contains("@@ -1,1 +1,1 @@"));
        assert!(rendered.contains("-old\n+new"));
    }

    #[test]
    fn nested_patch_artifact_is_attached_to_its_command_operation() {
        let diff = FileDiff {
            old_path: Some(PathBuf::from("src/main.rs")),
            new_path: Some(PathBuf::from("src/main.rs")),
            hunks: vec![atra_patch_types::DiffHunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![atra_patch_types::DiffLine {
                    kind: DiffLineKind::Added,
                    old_line: None,
                    new_line: Some(1),
                    text: "new".to_owned(),
                }],
            }],
        };
        let artifact = ToolArtifact::RunnerOperation(atra_protocol::RunnerOperationArtifact {
            operation: 1,
            runner: "sandbox".to_owned(),
            label: "Patch".to_owned(),
            result: Value::Null,
            artifacts: vec![ToolArtifact::PatchOperations(
                ApplyPatchResult::Operations {
                    results: vec![PatchOperationResult::Updated {
                        path: PathBuf::from("src/main.rs"),
                        outcome: PatchOperationOutcome::Applied { diff: Ok(diff) },
                    }],
                },
            )],
        });
        let mut operations = vec![CommandOperationDisplay {
            runner: "sandbox".to_owned(),
            command: "atri patch".to_owned(),
            output: String::new(),
            status: OperationStatus::Queued,
            omitted_bytes: 0,
            diff_files: SnapshotDiff::default(),
            file_changes: Vec::new(),
        }];
        apply_command_artifact(&artifact, &mut operations);
        assert_eq!(operations[0].diff_files.len(), 1);
        assert_eq!(
            operations[0]
                .diff_files
                .file(0)
                .and_then(|file| file.new_path.as_deref()),
            Some("src/main.rs")
        );
        assert_eq!(operations[0].file_changes.len(), 1);
        let change = &operations[0].file_changes[0];
        assert_eq!(change.path, "src/main.rs");
        assert_eq!(change.operation, "updated");
        assert_eq!(change.added, 1);
        assert_eq!(change.deleted, 0);
    }

    #[test]
    fn raw_items_keep_event_then_active_item_order() {
        let mut state = state(vec![
            event(1, "user_message", json!({"content": "prompt"})),
            event(
                2,
                "assistant_message",
                json!({"content": "working", "phase": "commentary"}),
            ),
        ]);
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(7),
                ActiveItemData::Assistant {
                    content: "streaming".to_owned(),
                    phase: AssistantMessagePhase::FinalAnswer,
                },
            ),
        }
        .apply(&mut state)
        .unwrap();

        assert_eq!(
            raw_keys(&state),
            vec![
                RawKey::Event(EventSequence(1)),
                RawKey::Event(EventSequence(2)),
                RawKey::Active(ActiveItemId(7)),
            ]
        );
        let raw = raw_item(&state, &RawKey::Active(ActiveItemId(7))).unwrap();
        assert!(raw.contains("\"content\": \"streaming\""));
    }

    #[test]
    fn compaction_starts_a_continued_turn() {
        let mut state = state(vec![
            event(
                1,
                "compaction",
                json!({"replacement": {"type": "summary", "content": "Earlier summary"}, "checkpoint_id": 1}),
            ),
            event(
                2,
                "assistant_message",
                json!({"content": "continued work", "phase": "commentary"}),
            ),
        ]);
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();

        assert_eq!(turn_keys(&state), vec![TurnKey(EventSequence(1))]);
        let turn = turn(&state, TurnKey(EventSequence(1))).unwrap();
        assert_eq!(
            turn.activity_keys(),
            vec![
                ActivityKey::Event(EventSequence(1)),
                ActivityKey::Event(EventSequence(2))
            ]
        );
    }

    #[test]
    fn compacted_turn_remains_visible_after_the_active_turn_finishes() {
        let state = state(vec![
            event(
                1,
                "compaction",
                json!({"replacement": {"type": "summary", "content": "Earlier summary"}, "checkpoint_id": 1}),
            ),
            event(
                2,
                "assistant_message",
                json!({"content": "continued work", "phase": "commentary"}),
            ),
        ]);

        assert_eq!(turn_keys(&state), vec![TurnKey(EventSequence(1))]);
        assert_eq!(
            turn(&state, TurnKey(EventSequence(1)))
                .unwrap()
                .activity_keys(),
            vec![
                ActivityKey::Event(EventSequence(1)),
                ActivityKey::Event(EventSequence(2))
            ]
        );
    }

    #[test]
    fn completed_turn_collapses_to_a_structural_summary() {
        let state = state(vec![
            event(1, "user_message", json!({"content": "run"})),
            event(
                2,
                "tool_call",
                json!({
                    "type": "function",
                    "call_id": "call-1",
                    "name": "command",
                    "arguments": {"command": "*** Runner sandbox\necho hello"}
                }),
            ),
            event(
                3,
                "tool_result",
                json!({
                    "type": "function",
                    "call_id": "call-1",
                    "name": "command",
                    "result": {"output": "hello"},
                    "artifacts": []
                }),
            ),
            event(
                4,
                "assistant_message",
                json!({"content": "working", "phase": "commentary"}),
            ),
        ]);
        let turn = turn(&state, TurnKey(EventSequence(1))).unwrap();
        let keys = turn.activity_keys();
        assert_eq!(activity_summary(&state, &keys), "1 command · 1 update");
    }

    #[test]
    fn active_turn_surfaces_the_current_activity_summary() {
        let mut state = state(vec![event(1, "user_message", json!({"content": "run"}))]);
        ThreadOperation::ActiveTurnStarted {
            phase: TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        ThreadOperation::ActiveItemAdded {
            item: ActiveItem::new(
                ActiveItemId(7),
                ActiveItemData::ToolCall {
                    item_id: "item-1".to_owned(),
                    call_id: Some("call-1".to_owned()),
                    name: "command".to_owned(),
                    input: "*** Runner sandbox\necho hello".to_owned(),
                },
            ),
        }
        .apply(&mut state)
        .unwrap();

        let turn = turn(&state, TurnKey(EventSequence(1))).unwrap();
        let keys = turn.activity_keys();
        assert_eq!(activity_summary(&state, &keys), "echo hello — sandbox");
    }
}
