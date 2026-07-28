use std::collections::{BTreeMap, HashMap};

use atra_protocol::{
    EventSequence, RunnerOperationUpdate, ThreadEvent, ThreadEventData, ToolCallEvent,
    ToolResultEvent,
};
use ratatui::text::Line;

mod render;

pub(crate) use atra_protocol::ToolArtifact;
pub(crate) use render::{
    layout_transcript, prepare_transcript, transcript_lines, transcript_ranges, transcript_text,
};

pub(crate) struct TranscriptState {
    pub(crate) entries: Vec<TranscriptEntry>,
    pub(crate) events: Vec<ThreadEvent>,
    tool_call_previews: HashMap<String, usize>,
}

impl TranscriptState {
    pub(crate) fn new(entries: Vec<TranscriptEntry>, events: Vec<ThreadEvent>) -> Self {
        Self {
            entries,
            events,
            tool_call_previews: HashMap::new(),
        }
    }

    pub(crate) fn replace(&mut self, entries: Vec<TranscriptEntry>, events: Vec<ThreadEvent>) {
        self.entries = entries;
        self.events = events;
        self.tool_call_previews.clear();
    }

    pub(crate) fn replace_events(&mut self, events: Vec<ThreadEvent>) {
        self.replace(transcript_from_events(&events), events);
    }

    pub(crate) fn clear(&mut self) {
        self.replace(Vec::new(), Vec::new());
    }

    pub(crate) fn append_assistant_delta(&mut self, content: &str) {
        let content = sanitize(content);
        if self
            .entries
            .last()
            .is_some_and(TranscriptEntry::is_assistant_message)
        {
            self.entries.last_mut().unwrap().append_message(&content);
        } else {
            self.entries
                .push(TranscriptEntry::message(Author::Assistant, content));
        }
    }

    pub(crate) fn append_reasoning_delta(&mut self, content: &str) {
        let content = sanitize(content);
        if self
            .entries
            .last()
            .is_some_and(TranscriptEntry::is_reasoning_summary)
        {
            self.entries.last_mut().unwrap().append_message(&content);
        } else {
            self.entries
                .push(TranscriptEntry::new(TranscriptItem::ReasoningSummary {
                    text: content,
                }));
        }
    }

    pub(crate) fn finish_reasoning_part(&mut self) {
        if let Some(entry) = self.entries.last_mut()
            && entry.is_reasoning_summary()
            && !entry.is_empty_reasoning_summary()
        {
            entry.append_message("\n\n");
        }
    }

    pub(crate) fn start_tool_preview(&mut self, item_id: String, name: &str) {
        let index = self.entries.len();
        self.entries
            .push(TranscriptEntry::new(TranscriptItem::ToolCall {
                name: sanitize(name),
                arguments: None,
            }));
        self.tool_call_previews.insert(item_id, index);
    }

    pub(crate) fn append_tool_preview(&mut self, item_id: &str, content: &str) {
        if let Some(index) = self.tool_call_previews.get(item_id) {
            self.entries[*index].append_tool_input(&sanitize(content));
        }
    }

    pub(crate) fn discard_tool_previews(&mut self) {
        let mut indices = self
            .tool_call_previews
            .drain()
            .map(|(_, index)| index)
            .collect::<Vec<_>>();
        indices.sort_unstable_by(|left, right| right.cmp(left));
        for index in indices {
            self.entries.remove(index);
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

    pub(crate) fn apply_event(&mut self, event: ThreadEvent) {
        if let Some(existing) = self
            .events
            .iter_mut()
            .find(|existing| existing.sequence == event.sequence)
        {
            *existing = event.clone();
            if merge_runner_tool_result(&mut self.entries, &event) {
                return;
            }
            if let Some(item) = item_from_event(event)
                && let Some(entry) = self
                    .entries
                    .iter_mut()
                    .find(|entry| entry.sequence == Some(existing.sequence))
            {
                entry.replace(item);
            }
            return;
        }
        self.events.push(event.clone());
        let item_id = match &event.data {
            ThreadEventData::ToolCall(ToolCallEvent::Custom { item_id, .. }) => item_id.clone(),
            _ => None,
        };
        let sequence = event.sequence;
        if merge_runner_tool_result(&mut self.entries, &event) {
            return;
        }
        let Some(item) = item_from_event(event) else {
            return;
        };
        if matches!(
            item,
            TranscriptItem::ToolCall { .. } | TranscriptItem::RunnerTool { .. }
        ) && let Some(item_id) = item_id
            && let Some(index) = self.tool_call_previews.remove(&item_id)
        {
            self.entries[index].replace_event(sequence, item);
            return;
        }
        if matches!(
            item,
            TranscriptItem::Message {
                author: Author::Assistant,
                ..
            }
        ) && let Some(entry) = self.entries.last_mut()
            && entry.is_assistant_message()
            && entry.sequence.is_none()
        {
            entry.replace_event(sequence, item);
            return;
        }
        self.entries.push(TranscriptEntry {
            item,
            sequence: Some(sequence),
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
        activity: RunnerActivity,
        output: String,
        omitted_bytes: usize,
    },
    Completed(ToolArtifact),
}

#[derive(Clone, Copy)]
pub(crate) enum RunnerActivity {
    Running,
    Waiting,
}

impl TranscriptItem {
    pub(crate) fn message(author: Author, text: String) -> Self {
        Self::Message { author, text }
    }

    pub(crate) fn append_message(&mut self, content: &str) {
        match self {
            Self::Message { text, .. } | Self::ReasoningSummary { text } => {
                text.push_str(content);
            }
            _ => unreachable!(),
        };
    }

    pub(crate) fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult { .. })
            || matches!(self, Self::RunnerTool { results, .. } if !results.is_empty())
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

    pub(crate) fn is_reasoning_summary(&self) -> bool {
        matches!(self, Self::ReasoningSummary { .. })
    }

    pub(crate) fn is_empty_reasoning_summary(&self) -> bool {
        matches!(self, Self::ReasoningSummary { text } if text.is_empty())
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

    pub(crate) fn message(author: Author, text: String) -> Self {
        Self::new(TranscriptItem::message(author, text))
    }

    pub(crate) fn append_message(&mut self, content: &str) {
        self.item.append_message(content);
        self.rendered = None;
    }

    pub(crate) fn append_tool_input(&mut self, content: &str) {
        match &mut self.item {
            TranscriptItem::RunnerTool { input, .. } => input.push_str(content),
            TranscriptItem::ToolCall { arguments, .. } => match arguments {
                Some(serde_json::Value::String(input)) => input.push_str(content),
                None => *arguments = Some(serde_json::Value::String(content.to_owned())),
                Some(_) => unreachable!(),
            },
            _ => unreachable!(),
        }
        self.rendered = None;
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
            RunnerOperationUpdate::CommandStarted => {
                results.insert(
                    operation_index,
                    RunnerResult::Running {
                        activity: RunnerActivity::Running,
                        output: String::new(),
                        omitted_bytes: 0,
                    },
                );
            }
            RunnerOperationUpdate::WaitStarted => {
                results.insert(
                    operation_index,
                    RunnerResult::Running {
                        activity: RunnerActivity::Waiting,
                        output: String::new(),
                        omitted_bytes: 0,
                    },
                );
            }
            RunnerOperationUpdate::CommandOutput {
                content,
                omitted_bytes,
            } => {
                let result =
                    results
                        .entry(operation_index)
                        .or_insert_with(|| RunnerResult::Running {
                            activity: RunnerActivity::Running,
                            output: String::new(),
                            omitted_bytes: 0,
                        });
                let RunnerResult::Running {
                    activity: _,
                    output,
                    omitted_bytes: total_omitted,
                } = result
                else {
                    return true;
                };
                output.push_str(&sanitize(&content));
                *total_omitted += omitted_bytes;
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

    pub(crate) fn replace_event(&mut self, sequence: EventSequence, item: TranscriptItem) {
        self.sequence = Some(sequence);
        self.replace(item);
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
            } => Some(text),
            _ => None,
        }
    }

    pub(crate) fn is_reasoning_summary(&self) -> bool {
        self.item.is_reasoning_summary()
    }

    pub(crate) fn is_empty_reasoning_summary(&self) -> bool {
        self.item.is_empty_reasoning_summary()
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
        ThreadEventData::AssistantMessage(message) => Some(TranscriptItem::message(
            Author::Assistant,
            sanitize(&message.content),
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
            if name == "runner"
                && let serde_json::Value::String(input) = arguments
            {
                Some(TranscriptItem::RunnerTool {
                    call_id: sanitize(call_id.as_deref()?),
                    input,
                    results: BTreeMap::new(),
                    pending_approval: None,
                    masked: false,
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
        ThreadEventData::WorkspaceInstructions(_)
        | ThreadEventData::Skills(_)
        | ThreadEventData::Runners(_)
        | ThreadEventData::FrozenBoundary(_)
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
    if name != "runner" {
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

pub(crate) fn transcript_from_events(events: &[ThreadEvent]) -> Vec<TranscriptEntry> {
    let mut transcript = Vec::new();
    for event in events {
        if merge_runner_tool_result(&mut transcript, event) {
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
    fn event_conversion_sanitizes_nested_tool_input() {
        let event = ThreadEvent {
            sequence: EventSequence(1),
            data: ThreadEventData::ToolCall(ToolCallEvent::Function {
                name: "exec\u{1b}[31m_command".to_owned(),
                arguments: serde_json::json!({
                    "command": "safe\u{1b}]52;c;bad\u{7}"
                }),
                call_id: None,
            }),
        };

        let Some(TranscriptItem::ToolCall { name, arguments }) = item_from_event(event) else {
            panic!("tool call event was not converted");
        };
        assert_eq!(name, "exec_command");
        assert_eq!(arguments.unwrap()["command"], "safe");
    }
}
