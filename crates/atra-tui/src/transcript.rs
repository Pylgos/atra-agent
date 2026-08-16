use std::collections::{BTreeMap, HashMap};

use atra_protocol::{
    ActiveItemData, ActiveItemId, EventSequence, RunnerOperationUpdate, ThreadChange, ThreadEvent,
    ThreadEventData, ThreadState, TodoItem, ToolCallEvent, ToolResultEvent,
};
use ratatui::text::Line;

mod render;

pub(crate) use atra_protocol::ToolArtifact;
pub(crate) use render::{
    layout_transcript, prepare_transcript, transcript_lines, transcript_ranges, transcript_text,
};

pub(crate) struct TranscriptState {
    pub(crate) entries: Vec<TranscriptEntry>,
    active_entries: HashMap<ActiveItemId, ActiveEntry>,
}

enum ActiveEntry {
    Transcript(usize),
    RunnerCall(String),
}

impl TranscriptState {
    pub(crate) fn new(entries: Vec<TranscriptEntry>) -> Self {
        Self {
            entries,
            active_entries: HashMap::new(),
        }
    }

    pub(crate) fn replace(&mut self, entries: Vec<TranscriptEntry>) {
        self.entries = entries;
        self.active_entries.clear();
    }

    pub(crate) fn rebuild(&mut self, state: &ThreadState) {
        self.replace(transcript_from_events(state.events()));
        if let Some(turn) = state.active_turn() {
            for item in turn.items() {
                self.synchronize_active(item.id(), item.data());
            }
            if let Some(approval) = turn.pending_approval() {
                self.set_pending_approval(approval.operation_index());
            }
        }
    }

    pub(crate) fn replace_events(&mut self, events: &[ThreadEvent]) {
        self.replace(transcript_from_events(events));
    }

    pub(crate) fn clear(&mut self) {
        self.replace(Vec::new());
    }

    pub(crate) fn apply_change(&mut self, state: &ThreadState, change: &ThreadChange) {
        match change {
            ThreadChange::Event(sequence) => {
                if let Some(event) = state
                    .events()
                    .iter()
                    .find(|event| event.sequence == *sequence)
                {
                    self.append_event(event);
                }
            }
            ThreadChange::ActiveItem(id) => match state
                .active_turn()
                .and_then(|turn| turn.items().iter().find(|item| item.id() == *id))
            {
                Some(item) => self.synchronize_active(*id, item.data()),
                None => self.remove_active(*id, state),
            },
            ThreadChange::ActiveItemFinalized {
                active_id,
                sequence,
            } => {
                self.remove_active(*active_id, state);
                if let Some(event) = state
                    .events()
                    .iter()
                    .find(|event| event.sequence == *sequence)
                {
                    self.append_event(event);
                }
            }
            ThreadChange::HistoryReplaced => self.rebuild(state),
            ThreadChange::Interaction => {
                if let Some(approval) = state.active_turn().and_then(|turn| turn.pending_approval())
                {
                    self.set_pending_approval(approval.operation_index());
                } else {
                    self.set_pending_approval(None);
                }
            }
            ThreadChange::TurnFinished => {
                let active_ids = self.active_entries.keys().copied().collect::<Vec<_>>();
                for id in active_ids {
                    self.remove_active(id, state);
                }
                self.set_pending_approval(None);
            }
            ThreadChange::Metadata
            | ThreadChange::Phase
            | ThreadChange::Checkpoint(_)
            | ThreadChange::Process(_) => {}
        }
    }

    fn synchronize_active(&mut self, id: ActiveItemId, data: &ActiveItemData) {
        if let ActiveItemData::RunnerTool {
            call_id,
            operation_index,
            update,
        } = data
        {
            self.active_entries
                .insert(id, ActiveEntry::RunnerCall(call_id.clone()));
            self.update_runner_operation(call_id, *operation_index, update.clone());
            return;
        }
        let item = match data {
            ActiveItemData::Assistant { content } => TranscriptItem::Message {
                author: Author::Assistant,
                text: sanitize(content),
                todos: Vec::new(),
            },
            ActiveItemData::Reasoning { content } => TranscriptItem::ReasoningSummary {
                text: sanitize(content),
            },
            ActiveItemData::WebSearch { action, .. } => TranscriptItem::WebSearch {
                action: sanitize_value(action.clone().unwrap_or(serde_json::Value::Null)),
            },
            ActiveItemData::ToolCall { name, input, .. } => TranscriptItem::ToolCall {
                name: sanitize(name),
                arguments: (!input.is_empty()).then(|| {
                    sanitize_value(
                        serde_json::from_str(input)
                            .unwrap_or_else(|_| serde_json::Value::String(input.clone())),
                    )
                }),
            },
            ActiveItemData::RunnerTool { .. } => unreachable!(),
        };
        match self.active_entries.get(&id) {
            Some(ActiveEntry::Transcript(index)) => self.entries[*index].replace(item),
            Some(ActiveEntry::RunnerCall(_)) => unreachable!("active item kind changed"),
            None => {
                let index = self.entries.len();
                self.entries.push(TranscriptEntry::new(item));
                self.active_entries
                    .insert(id, ActiveEntry::Transcript(index));
            }
        }
    }

    fn remove_active(&mut self, id: ActiveItemId, state: &ThreadState) {
        let Some(active) = self.active_entries.remove(&id) else {
            return;
        };
        let index = match active {
            ActiveEntry::Transcript(index) => index,
            ActiveEntry::RunnerCall(call_id) => {
                self.restore_runner_call(state, &call_id);
                return;
            }
        };
        self.entries.remove(index);
        for current in self.active_entries.values_mut() {
            if let ActiveEntry::Transcript(current) = current
                && *current > index
            {
                *current -= 1;
            }
        }
    }

    fn restore_runner_call(&mut self, state: &ThreadState, call_id: &str) {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.runner_call_id() == Some(call_id))
        else {
            return;
        };
        let Some(event) = state.events().iter().find(|event| {
            matches!(
                &event.data,
                ThreadEventData::ToolCall(ToolCallEvent::Custom {
                    call_id: current,
                    ..
                } | ToolCallEvent::Function {
                    call_id: Some(current),
                    ..
                }) if current == call_id
            )
        }) else {
            return;
        };
        let Some(mut entry) = TranscriptEntry::from_event(event.clone()) else {
            return;
        };
        for event in state.events() {
            merge_runner_tool_result(std::slice::from_mut(&mut entry), event);
        }
        self.entries[index] = entry;
        if let Some(turn) = state.active_turn() {
            for item in turn.items() {
                if let ActiveItemData::RunnerTool {
                    call_id: current,
                    operation_index,
                    update,
                } = item.data()
                    && current == call_id
                {
                    self.update_runner_operation(current, *operation_index, update.clone());
                }
            }
        }
    }

    pub(crate) fn update_runner_operation(
        &mut self,
        call_id: &str,
        operation_index: usize,
        update: RunnerOperationUpdate,
    ) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.runner_call_id() == Some(call_id))
        {
            entry.update_runner_operation(call_id, operation_index, update);
        }
    }

    pub(crate) fn set_pending_approval(&mut self, operation_index: Option<usize>) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.runner_call_id().is_some())
        {
            entry.set_pending_approval(operation_index);
        }
    }

    fn append_event(&mut self, event: &ThreadEvent) {
        if merge_tool_result(&mut self.entries, event) {
            return;
        }
        let Some(item) = item_from_event(event.clone()) else {
            return;
        };
        self.entries.push(TranscriptEntry {
            item,
            sequence: Some(event.sequence),
            rendered: None,
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Author {
    User,
    Assistant,
}

#[derive(Clone)]
pub(crate) enum TranscriptItem {
    Message {
        author: Author,
        text: String,
        todos: Vec<TodoItem>,
    },
    ReasoningSummary {
        text: String,
    },
    WebSearch {
        action: serde_json::Value,
    },
    ToolCall {
        name: String,
        arguments: Option<serde_json::Value>,
    },
    Question {
        call_id: Option<String>,
        arguments: serde_json::Value,
        answers: Option<Vec<atra_protocol::QuestionAnswer>>,
    },
    RunnerTool {
        call_id: String,
        input: String,
        results: BTreeMap<usize, RunnerResult>,
        pending_approval: Option<usize>,
        masked: bool,
    },
    ToolResult {
        artifacts: Vec<ToolArtifact>,
        masked: bool,
    },
    Compaction,
}

#[derive(Clone)]
pub(crate) enum RunnerResult {
    Running {
        output: String,
        omitted_bytes: usize,
        timer: atra_protocol::CommandTimerState,
    },
    Completed(ToolArtifact),
}

impl TranscriptItem {
    pub(crate) fn message(author: Author, text: String) -> Self {
        Self::Message {
            author,
            text,
            todos: Vec::new(),
        }
    }

    pub(crate) fn assistant_message(text: String, todos: Vec<TodoItem>) -> Self {
        Self::Message {
            author: Author::Assistant,
            text,
            todos,
        }
    }

    pub(crate) fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult { .. })
            || matches!(self, Self::RunnerTool { results, .. } if !results.is_empty())
    }

    pub(crate) fn is_tool_call(&self) -> bool {
        matches!(self, Self::ToolCall { .. } | Self::Question { .. })
    }

    pub(crate) fn is_user_message(&self) -> bool {
        matches!(
            self,
            Self::Message {
                author: Author::User,
                ..
            }
        )
    }

    pub(crate) fn is_assistant_message(&self) -> bool {
        matches!(
            self,
            Self::Message {
                author: Author::Assistant,
                ..
            }
        )
    }
}

pub(crate) struct TranscriptEntry {
    pub(crate) item: TranscriptItem,
    pub(crate) sequence: Option<EventSequence>,
    pub(crate) rendered: Option<RenderedItem>,
}

impl TranscriptEntry {
    pub(crate) fn new(item: TranscriptItem) -> Self {
        Self {
            item,
            sequence: None,
            rendered: None,
        }
    }

    pub(crate) fn from_event(event: ThreadEvent) -> Option<Self> {
        let sequence = event.sequence;
        Some(Self {
            item: item_from_event(event)?,
            sequence: Some(sequence),
            rendered: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn message(author: Author, text: String) -> Self {
        Self::new(TranscriptItem::message(author, text))
    }

    pub(crate) fn update_runner_operation(
        &mut self,
        call_id: &str,
        operation_index: usize,
        update: RunnerOperationUpdate,
    ) -> bool {
        let TranscriptItem::RunnerTool {
            call_id: entry_call_id,
            results,
            ..
        } = &mut self.item
        else {
            return false;
        };
        if entry_call_id != call_id {
            return false;
        }
        match update {
            RunnerOperationUpdate::CommandStarted { timer } => {
                results.insert(
                    operation_index,
                    RunnerResult::Running {
                        output: String::new(),
                        omitted_bytes: 0,
                        timer,
                    },
                );
            }
            RunnerOperationUpdate::CommandOutput {
                content,
                omitted_bytes,
                timer,
            } => {
                let result =
                    results
                        .entry(operation_index)
                        .or_insert_with(|| RunnerResult::Running {
                            output: String::new(),
                            omitted_bytes: 0,
                            timer: timer.clone(),
                        });
                let RunnerResult::Running {
                    output,
                    omitted_bytes: total_omitted,
                    timer: current_timer,
                } = result
                else {
                    return true;
                };
                output.push_str(&sanitize(&content));
                *total_omitted += omitted_bytes;
                *current_timer = timer;
                truncate_live_output(output, total_omitted);
            }
            RunnerOperationUpdate::Completed { artifact } => {
                results.insert(
                    operation_index,
                    RunnerResult::Completed(sanitize_artifact(artifact)),
                );
            }
        }
        self.rendered = None;
        true
    }

    pub(crate) fn set_pending_approval(&mut self, operation: Option<usize>) -> bool {
        let TranscriptItem::RunnerTool {
            pending_approval, ..
        } = &mut self.item
        else {
            return false;
        };
        *pending_approval = operation;
        self.rendered = None;
        true
    }

    pub(crate) fn runner_call_id(&self) -> Option<&str> {
        match &self.item {
            TranscriptItem::RunnerTool { call_id, .. } => Some(call_id),
            _ => None,
        }
    }

    pub(crate) fn replace(&mut self, item: TranscriptItem) {
        self.item = item;
        self.rendered = None;
    }

    pub(crate) fn is_tool_result(&self) -> bool {
        self.item.is_tool_result()
    }

    pub(crate) fn is_assistant_message(&self) -> bool {
        self.item.is_assistant_message()
    }

    pub(crate) fn user_message(&self) -> Option<&str> {
        match &self.item {
            TranscriptItem::Message {
                author: Author::User,
                text,
                ..
            } => Some(text),
            _ => None,
        }
    }
}

pub(crate) struct RenderedItem {
    pub(crate) width: u16,
    pub(crate) expanded: bool,
    pub(crate) lines: Vec<DisplayedLine>,
}

#[derive(Clone)]
pub(crate) struct DisplayedLine {
    pub(crate) marker: Option<char>,
    pub(crate) line: Line<'static>,
    pub(crate) continuation: bool,
}

pub(crate) fn item_from_event(event: ThreadEvent) -> Option<TranscriptItem> {
    match event.data {
        ThreadEventData::UserMessage(message) => Some(TranscriptItem::message(
            Author::User,
            sanitize(&message.content),
        )),
        ThreadEventData::AssistantMessage(message) => Some(TranscriptItem::assistant_message(
            sanitize(&message.content),
            message
                .todos
                .into_iter()
                .map(|todo| TodoItem {
                    step: sanitize(&todo.step),
                    status: todo.status,
                })
                .collect(),
        )),
        ThreadEventData::Reasoning(reasoning) => {
            let summary = reasoning.item.pointer("/summary")?.as_array()?;
            let text = summary
                .iter()
                .filter_map(|part| part.get("text")?.as_str())
                .map(sanitize)
                .collect::<Vec<_>>()
                .join("\n\n");
            (!text.is_empty()).then_some(TranscriptItem::ReasoningSummary { text })
        }
        ThreadEventData::WebSearch(search) => Some(TranscriptItem::WebSearch {
            action: sanitize_value(
                search
                    .item
                    .get("action")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            ),
        }),
        ThreadEventData::ToolCall(call) => {
            let (name, arguments, call_id) = match call {
                ToolCallEvent::Custom {
                    name,
                    input,
                    call_id,
                    ..
                } => (name, serde_json::Value::String(input), Some(call_id)),
                ToolCallEvent::Function {
                    name,
                    arguments,
                    call_id,
                } => (name, arguments, call_id),
            };
            let name = sanitize(&name);
            let arguments = sanitize_value(arguments);
            if name == "command"
                && let serde_json::Value::String(input) = arguments
            {
                Some(TranscriptItem::RunnerTool {
                    call_id: sanitize(call_id.as_deref()?),
                    input,
                    results: BTreeMap::new(),
                    pending_approval: None,
                    masked: false,
                })
            } else if name == "question" {
                Some(TranscriptItem::Question {
                    call_id: call_id.as_deref().map(sanitize),
                    arguments,
                    answers: None,
                })
            } else {
                Some(TranscriptItem::ToolCall {
                    name,
                    arguments: Some(arguments),
                })
            }
        }
        ThreadEventData::ToolResult(result) => {
            let (result, artifacts, masked_result) = match result {
                ToolResultEvent::Custom {
                    result,
                    artifacts,
                    masked_result,
                    ..
                }
                | ToolResultEvent::Function {
                    result,
                    artifacts,
                    masked_result,
                    ..
                } => (result, artifacts, masked_result),
            };
            let masked = masked_result
                .as_ref()
                .is_some_and(|masked| masked != &result);
            Some(TranscriptItem::ToolResult {
                artifacts: artifacts.iter().cloned().map(sanitize_artifact).collect(),
                masked,
            })
        }
        ThreadEventData::Compaction(_) => Some(TranscriptItem::Compaction),
        ThreadEventData::ThreadContext(_)
        | ThreadEventData::WorkspaceInstructions(_)
        | ThreadEventData::Skills(_)
        | ThreadEventData::Runners(_)
        | ThreadEventData::FrozenBoundary(_)
        | ThreadEventData::ModelOutput(_)
        | ThreadEventData::ModelRequest(_)
        | ThreadEventData::TokenUsage(_)
        | ThreadEventData::RateLimits(_) => None,
    }
}

pub(crate) fn merge_runner_tool_result(
    transcript: &mut [TranscriptEntry],
    event: &ThreadEvent,
) -> bool {
    let ThreadEventData::ToolResult(result) = &event.data else {
        return false;
    };
    let (name, call_id, result, artifacts, masked_result) = match result {
        ToolResultEvent::Custom {
            name,
            call_id,
            result,
            artifacts,
            masked_result,
            ..
        }
        | ToolResultEvent::Function {
            name,
            call_id,
            result,
            artifacts,
            masked_result,
            ..
        } => (name, call_id, result, artifacts, masked_result),
    };
    if name != "command" {
        return false;
    }
    let Some(call_id) = call_id.as_deref() else {
        return false;
    };
    let Some(entry) = transcript.iter_mut().rev().find(|entry| {
        matches!(
            &entry.item,
            TranscriptItem::RunnerTool {
                call_id: entry_call_id,
                ..
            } if entry_call_id == call_id
        )
    }) else {
        return false;
    };
    let TranscriptItem::RunnerTool {
        results, masked, ..
    } = &mut entry.item
    else {
        unreachable!();
    };
    for artifact in artifacts.iter().cloned().map(sanitize_artifact) {
        match &artifact {
            ToolArtifact::RunnerOperation(operation) => {
                results.insert(operation.operation, RunnerResult::Completed(artifact));
            }
            ToolArtifact::CommandExecution(_) | ToolArtifact::PatchOperations(_) => {}
        }
    }
    *masked = masked_result
        .as_ref()
        .is_some_and(|masked| masked != result);
    entry.rendered = None;
    true
}

fn merge_question_tool_result(transcript: &mut [TranscriptEntry], event: &ThreadEvent) -> bool {
    let ThreadEventData::ToolResult(result) = &event.data else {
        return false;
    };
    let (name, call_id, result) = match result {
        ToolResultEvent::Custom {
            name,
            call_id,
            result,
            ..
        }
        | ToolResultEvent::Function {
            name,
            call_id,
            result,
            ..
        } => (name, call_id, result),
    };
    if name != "question" {
        return false;
    }
    let Some(call_id) = call_id.as_deref() else {
        return false;
    };
    let Some(entry) = transcript.iter_mut().rev().find(|entry| {
        matches!(
            &entry.item,
            TranscriptItem::Question {
                call_id: Some(entry_call_id),
                ..
            } if entry_call_id == call_id
        )
    }) else {
        return false;
    };
    let Ok(answers) = serde_json::from_value::<Vec<atra_protocol::QuestionAnswer>>(sanitize_value(
        result.clone(),
    )) else {
        return false;
    };
    let TranscriptItem::Question {
        answers: entry_answers,
        ..
    } = &mut entry.item
    else {
        unreachable!();
    };
    *entry_answers = Some(answers);
    entry.rendered = None;
    true
}

fn merge_tool_result(transcript: &mut [TranscriptEntry], event: &ThreadEvent) -> bool {
    merge_runner_tool_result(transcript, event) || merge_question_tool_result(transcript, event)
}

pub(crate) fn transcript_from_events(events: &[ThreadEvent]) -> Vec<TranscriptEntry> {
    let mut transcript = Vec::new();
    for event in events {
        if merge_tool_result(&mut transcript, event) {
            continue;
        }
        if let Some(entry) = TranscriptEntry::from_event(event.clone()) {
            transcript.push(entry);
        }
    }
    transcript
}

fn sanitize_artifact(artifact: ToolArtifact) -> ToolArtifact {
    serde_json::from_value(sanitize_value(
        serde_json::to_value(artifact).expect("tool artifacts serialize"),
    ))
    .expect("sanitizing strings preserves tool artifact structure")
}

fn truncate_live_output(output: &mut String, omitted_bytes: &mut usize) {
    const MAX_LIVE_OUTPUT_BYTES: usize = 40_000;
    if output.len() <= MAX_LIVE_OUTPUT_BYTES {
        return;
    }
    let head_end = char_boundary_at_or_before(output, MAX_LIVE_OUTPUT_BYTES / 2);
    let tail_start =
        char_boundary_at_or_after(output, output.len() - MAX_LIVE_OUTPUT_BYTES / 2).max(head_end);
    *omitted_bytes += tail_start - head_end;
    output.replace_range(head_end..tail_start, "");
}

fn char_boundary_at_or_before(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn char_boundary_at_or_after(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn sanitize_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(value) => serde_json::Value::String(sanitize(&value)),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(sanitize_value).collect())
        }
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (sanitize(&key), sanitize_value(value)))
                .collect(),
        ),
        value => value,
    }
}

pub(crate) fn sanitize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            if characters.next_if_eq(&'[').is_some() {
                for next in characters.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            } else if characters
                .next_if(|next| matches!(next, ']' | 'P' | '^' | '_' | 'X'))
                .is_some()
            {
                while let Some(next) = characters.next() {
                    if next == '\x07' || (next == '\x1b' && characters.next_if_eq(&'\\').is_some())
                    {
                        break;
                    }
                }
            } else {
                characters.next();
            }
        } else if character == '\n' || character == '\t' || !character.is_control() {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_conversion_sanitizes_todos() {
        let event = ThreadEvent {
            sequence: EventSequence(1),
            data: ThreadEventData::AssistantMessage(atra_protocol::AssistantMessageEvent {
                content: "body".to_owned(),
                phase: None,
                todos: vec![TodoItem {
                    step: "safe\u{1b}]52;c;bad\u{7}".to_owned(),
                    status: atra_protocol::TodoStatus::Pending,
                }],
            }),
        };

        let Some(TranscriptItem::Message { todos, .. }) = item_from_event(event) else {
            panic!("assistant message event was not converted");
        };
        assert_eq!(todos[0].step, "safe");
    }

    #[test]
    fn question_results_merge_for_history_and_live_updates() {
        let call = ThreadEvent {
            sequence: EventSequence(1),
            data: ThreadEventData::ToolCall(ToolCallEvent::Function {
                name: "question".to_owned(),
                arguments: serde_json::json!({
                    "questions": [{
                        "question": "Choose",
                        "options": [{"label": "A", "description": ""}],
                        "recommended_options": []
                    }]
                }),
                call_id: Some("question-1".to_owned()),
            }),
        };
        let result = ThreadEvent {
            sequence: EventSequence(2),
            data: ThreadEventData::ToolResult(ToolResultEvent::Function {
                call_type: None,
                name: "question".to_owned(),
                call_id: Some("question-1".to_owned()),
                result: serde_json::json!([{
                    "selected_option": "A",
                    "note": "details"
                }]),
                artifacts: Vec::new(),
                masked_result: None,
            }),
        };

        let history = transcript_from_events(&[call.clone(), result.clone()]);
        assert_answered_question(&history);

        let mut live = TranscriptState::new(Vec::new());
        live.append_event(&call);
        live.append_event(&result);
        assert_answered_question(&live.entries);
    }

    fn assert_answered_question(entries: &[TranscriptEntry]) {
        assert_eq!(entries.len(), 1);
        let TranscriptItem::Question {
            answers: Some(answers),
            ..
        } = &entries[0].item
        else {
            panic!("question result should merge into the question call");
        };
        assert_eq!(answers[0].selected_option.as_deref(), Some("A"));
        assert_eq!(answers[0].note, "details");
    }

    #[test]
    fn finishing_a_turn_removes_uncommitted_active_entries() {
        let metadata = atra_protocol::Thread {
            id: atra_protocol::ThreadId(1),
            parent_thread_id: None,
            display_name: None,
            provider: "fake".to_owned(),
            model: "test".to_owned(),
            reasoning_effort: "medium".to_owned(),
        };
        let mut state =
            ThreadState::materialize(metadata, Vec::new(), Vec::new(), Vec::new()).unwrap();
        atra_protocol::ThreadOperation::ActiveTurnStarted {
            phase: atra_protocol::TurnPhase::Running,
        }
        .apply(&mut state)
        .unwrap();
        atra_protocol::ThreadOperation::ActiveItemAdded {
            item: atra_protocol::ActiveItem::new(
                ActiveItemId(1),
                ActiveItemData::Assistant {
                    content: "partial".to_owned(),
                },
            ),
        }
        .apply(&mut state)
        .unwrap();
        let mut transcript = TranscriptState::new(Vec::new());
        transcript.rebuild(&state);
        assert_eq!(transcript.entries.len(), 1);

        let change = atra_protocol::ThreadOperation::TurnFinished {
            outcome: atra_protocol::TurnOutcome::Failed {
                message: "failed".to_owned(),
            },
        }
        .apply(&mut state)
        .unwrap();
        transcript.apply_change(&state, &change);

        assert!(transcript.entries.is_empty());
        assert!(transcript.active_entries.is_empty());
    }
}
