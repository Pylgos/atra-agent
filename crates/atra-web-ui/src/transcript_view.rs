use std::fmt;
use std::{borrow::Cow, collections::HashMap};

use atra_protocol::{
    ActiveItemData, ActiveItemId, AssistantMessagePhase, EventSequence, ThreadEvent,
    ThreadEventData, ThreadState, TodoStatus,
};
use serde_json::Value;

use crate::model::pretty;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivityKind {
    Tool,
    Search,
    Reasoning,
    Commentary,
    Todo,
    Skill,
    Marker,
}

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
        result: Option<EventSequence>,
    },
    Todo {
        source: EventSequence,
        index: usize,
    },
    Active(ActiveItemId),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) enum RawKey {
    Event(EventSequence),
    Active(ActiveItemId),
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
            Self::Tool { call, .. } => write!(formatter, "tool-{}", call.0),
            Self::Todo { source, index } => write!(formatter, "todo-{}-{index}", source.0),
            Self::Active(id) => write!(formatter, "active-{}", id.0),
        }
    }
}

pub(super) struct TurnRef<'a> {
    state: &'a ThreadState,
    start: usize,
    end: usize,
    key: TurnKey,
}

pub(super) struct ActivityDisplay<'a> {
    pub title: Cow<'a, str>,
    pub body: Cow<'a, str>,
    pub kind: ActivityKind,
    pub active: bool,
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
    if !is_turn_boundary(&state.events()[start].data) {
        return None;
    }
    let end = state.events()[start + 1..]
        .iter()
        .position(|event| is_turn_boundary(&event.data))
        .map_or(state.events().len(), |offset| start + 1 + offset);
    Some(TurnRef {
        state,
        start,
        end,
        key,
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
        match &self.state.events()[self.start].data {
            ThreadEventData::UserMessage(message) => &message.content,
            ThreadEventData::Compaction(_) => "Compacted history",
            _ => unreachable!("turn start was validated"),
        }
    }

    pub fn prompt_sequence(&self) -> Option<EventSequence> {
        matches!(
            self.state.events()[self.start].data,
            ThreadEventData::UserMessage(_)
        )
        .then_some(self.key.sequence())
    }

    pub fn answer(&self) -> Option<(EventSequence, &'a str)> {
        self.events()
            .iter()
            .rev()
            .find_map(|event| match &event.data {
                ThreadEventData::AssistantMessage(message)
                    if message.phase != Some(AssistantMessagePhase::Commentary) =>
                {
                    Some((event.sequence, message.content.as_str()))
                }
                _ => None,
            })
    }

    pub fn activity_keys(&self) -> Vec<ActivityKey> {
        let mut activities = Vec::new();
        let mut pending_tools = HashMap::<String, usize>::new();

        for event in self.events() {
            match &event.data {
                ThreadEventData::AssistantMessage(message)
                    if message.phase == Some(AssistantMessagePhase::Commentary) =>
                {
                    activities.push(ActivityKey::Event(event.sequence));
                    append_todo_keys(&mut activities, event.sequence, message.todos.len());
                }
                ThreadEventData::AssistantMessage(message) => {
                    append_todo_keys(&mut activities, event.sequence, message.todos.len());
                }
                ThreadEventData::ToolCall(call) => {
                    let value = serde_json::to_value(call).unwrap_or(Value::Null);
                    let index = activities.len();
                    activities.push(ActivityKey::Tool {
                        call: event.sequence,
                        result: None,
                    });
                    if let Some(key) = call_key(&value) {
                        pending_tools.insert(key, index);
                    }
                }
                ThreadEventData::ToolResult(result) => {
                    let value = serde_json::to_value(result).unwrap_or(Value::Null);
                    if let Some(index) = call_key(&value).and_then(|key| pending_tools.remove(&key))
                        && let Some(ActivityKey::Tool { result, .. }) = activities.get_mut(index)
                    {
                        *result = Some(event.sequence);
                    } else {
                        activities.push(ActivityKey::Event(event.sequence));
                    }
                }
                ThreadEventData::Reasoning(_)
                | ThreadEventData::WebSearch(_)
                | ThreadEventData::SkillInvocation(_)
                | ThreadEventData::Compaction(_)
                | ThreadEventData::FrozenBoundary(_) => {
                    activities.push(ActivityKey::Event(event.sequence));
                }
                ThreadEventData::ThreadContext(_)
                | ThreadEventData::WorkspaceInstructions(_)
                | ThreadEventData::Skills(_)
                | ThreadEventData::Runners(_)
                | ThreadEventData::UserMessage(_)
                | ThreadEventData::ModelOutput(_)
                | ThreadEventData::ModelRequest(_)
                | ThreadEventData::TokenUsage(_)
                | ThreadEventData::RateLimits(_) => {}
            }
        }

        if self.is_last()
            && let Some(active) = self.state.active_turn()
        {
            activities.extend(
                active
                    .items()
                    .iter()
                    .map(|item| ActivityKey::Active(item.id())),
            );
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
) -> Option<ActivityDisplay<'a>> {
    match key {
        ActivityKey::Event(sequence) => {
            let event = event(state, *sequence)?;
            match &event.data {
                ThreadEventData::AssistantMessage(message) => Some(ActivityDisplay {
                    title: Cow::Borrowed("Commentary"),
                    body: Cow::Borrowed(&message.content),
                    kind: ActivityKind::Commentary,
                    active: false,
                }),
                ThreadEventData::ToolResult(result) => {
                    let value = serde_json::to_value(result).unwrap_or(Value::Null);
                    Some(ActivityDisplay {
                        title: Cow::Owned(
                            value
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("Tool result")
                                .to_owned(),
                        ),
                        body: Cow::Owned(pretty(&value)),
                        kind: ActivityKind::Tool,
                        active: false,
                    })
                }
                ThreadEventData::Reasoning(reasoning) => Some(ActivityDisplay {
                    title: Cow::Borrowed("Reasoned"),
                    body: Cow::Owned(pretty(&reasoning.item)),
                    kind: ActivityKind::Reasoning,
                    active: false,
                }),
                ThreadEventData::WebSearch(search) => Some(ActivityDisplay {
                    title: Cow::Borrowed("Web search"),
                    body: Cow::Owned(pretty(&search.item)),
                    kind: ActivityKind::Search,
                    active: false,
                }),
                ThreadEventData::SkillInvocation(skill) => Some(ActivityDisplay {
                    title: Cow::Owned(format!("Skill · {}", skill.name)),
                    body: Cow::Borrowed(&skill.path),
                    kind: ActivityKind::Skill,
                    active: false,
                }),
                ThreadEventData::Compaction(_) => Some(ActivityDisplay {
                    title: Cow::Borrowed("History compacted"),
                    body: Cow::Borrowed(""),
                    kind: ActivityKind::Marker,
                    active: false,
                }),
                ThreadEventData::FrozenBoundary(_) => Some(ActivityDisplay {
                    title: Cow::Borrowed("History boundary"),
                    body: Cow::Borrowed(""),
                    kind: ActivityKind::Marker,
                    active: false,
                }),
                _ => None,
            }
        }
        ActivityKey::Tool { call, result } => {
            let call = event(state, *call)?;
            let ThreadEventData::ToolCall(call) = &call.data else {
                return None;
            };
            let value = serde_json::to_value(call).unwrap_or(Value::Null);
            let mut body = pretty(&value);
            if let Some(result) = result.and_then(|sequence| event(state, sequence))
                && let ThreadEventData::ToolResult(result) = &result.data
            {
                let value = serde_json::to_value(result).unwrap_or(Value::Null);
                let visible = value
                    .get("masked_result")
                    .filter(|value| !value.is_null())
                    .or_else(|| value.get("result"))
                    .unwrap_or(&value);
                body.push_str("\n\nResult\n");
                body.push_str(&pretty(visible));
            }
            Some(ActivityDisplay {
                title: Cow::Owned(
                    value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("Tool")
                        .to_owned(),
                ),
                body: Cow::Owned(body),
                kind: ActivityKind::Tool,
                active: false,
            })
        }
        ActivityKey::Todo { source, index } => {
            let event = event(state, *source)?;
            let ThreadEventData::AssistantMessage(message) = &event.data else {
                return None;
            };
            let todo = message.todos.get(*index)?;
            Some(ActivityDisplay {
                title: Cow::Owned(format!(
                    "Todo · {}",
                    match todo.status {
                        TodoStatus::Pending => "Pending",
                        TodoStatus::InProgress => "In progress",
                        TodoStatus::Completed => "Completed",
                    }
                )),
                body: Cow::Borrowed(&todo.step),
                kind: ActivityKind::Todo,
                active: false,
            })
        }
        ActivityKey::Active(id) => {
            let item = state
                .active_turn()?
                .items()
                .iter()
                .find(|item| item.id() == *id)?;
            let (title, body, kind) = match item.data() {
                ActiveItemData::Assistant { content } => (
                    Cow::Borrowed("Assistant response"),
                    Cow::Borrowed(content.as_str()),
                    ActivityKind::Commentary,
                ),
                ActiveItemData::Reasoning { content } => (
                    Cow::Borrowed("Reasoning"),
                    Cow::Borrowed(content.as_str()),
                    ActivityKind::Reasoning,
                ),
                ActiveItemData::WebSearch { action, .. } => (
                    Cow::Borrowed("Web search"),
                    action
                        .as_ref()
                        .map(|action| Cow::Owned(pretty(action)))
                        .unwrap_or_else(|| Cow::Borrowed("Searching…")),
                    ActivityKind::Search,
                ),
                ActiveItemData::ToolCall { name, input, .. } => (
                    Cow::Borrowed(name.as_str()),
                    Cow::Borrowed(input.as_str()),
                    ActivityKind::Tool,
                ),
                ActiveItemData::RunnerTool { update, .. } => (
                    Cow::Borrowed("Runner operation"),
                    Cow::Owned(pretty(update)),
                    ActivityKind::Tool,
                ),
            };
            Some(ActivityDisplay {
                title,
                body,
                kind,
                active: true,
            })
        }
    }
}

pub(super) fn activity_summary(state: &ThreadState, keys: &[ActivityKey]) -> String {
    let mut tools = 0;
    let mut searches = 0;
    let mut reasoning = 0;
    let mut other = 0;
    for key in keys {
        if let Some(kind) = activity_kind(state, key) {
            match kind {
                ActivityKind::Tool => tools += 1,
                ActivityKind::Search => searches += 1,
                ActivityKind::Reasoning => reasoning += 1,
                ActivityKind::Commentary
                | ActivityKind::Todo
                | ActivityKind::Skill
                | ActivityKind::Marker => other += 1,
            }
        }
    }
    let mut parts = Vec::new();
    if tools > 0 {
        parts.push(format!("{tools} tool{}", if tools == 1 { "" } else { "s" }));
    }
    if searches > 0 {
        parts.push(format!(
            "{searches} search{}",
            if searches == 1 { "" } else { "es" }
        ));
    }
    if reasoning > 0 {
        parts.push("Reasoned".to_owned());
    }
    if other > 0 {
        parts.push(format!(
            "{other} update{}",
            if other == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        "No activity".to_owned()
    } else {
        parts.join(" · ")
    }
}

fn activity_kind(state: &ThreadState, key: &ActivityKey) -> Option<ActivityKind> {
    match key {
        ActivityKey::Event(sequence) => match &event(state, *sequence)?.data {
            ThreadEventData::AssistantMessage(_) => Some(ActivityKind::Commentary),
            ThreadEventData::ToolResult(_) => Some(ActivityKind::Tool),
            ThreadEventData::Reasoning(_) => Some(ActivityKind::Reasoning),
            ThreadEventData::WebSearch(_) => Some(ActivityKind::Search),
            ThreadEventData::SkillInvocation(_) => Some(ActivityKind::Skill),
            ThreadEventData::Compaction(_) | ThreadEventData::FrozenBoundary(_) => {
                Some(ActivityKind::Marker)
            }
            _ => None,
        },
        ActivityKey::Tool { .. } => Some(ActivityKind::Tool),
        ActivityKey::Todo { .. } => Some(ActivityKind::Todo),
        ActivityKey::Active(id) => {
            let item = state
                .active_turn()?
                .items()
                .iter()
                .find(|item| item.id() == *id)?;
            Some(match item.data() {
                ActiveItemData::Assistant { .. } => ActivityKind::Commentary,
                ActiveItemData::Reasoning { .. } => ActivityKind::Reasoning,
                ActiveItemData::WebSearch { .. } => ActivityKind::Search,
                ActiveItemData::ToolCall { .. } | ActiveItemData::RunnerTool { .. } => {
                    ActivityKind::Tool
                }
            })
        }
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

fn append_todo_keys(keys: &mut Vec<ActivityKey>, source: EventSequence, count: usize) {
    keys.extend((0..count).map(|index| ActivityKey::Todo { source, index }));
}

fn call_key(value: &Value) -> Option<String> {
    value
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| value.get("item_id").and_then(Value::as_str))
        .map(str::to_owned)
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
    use atra_protocol::{ActiveItem, ThreadOperation, TurnPhase};
    use serde_json::json;

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
                json!({"content": "answer", "todos": [
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
        assert_eq!(first.answer(), Some((EventSequence(3), "answer")));
        assert_eq!(
            first.activity_keys(),
            vec![
                ActivityKey::Event(EventSequence(2)),
                ActivityKey::Todo {
                    source: EventSequence(3),
                    index: 0,
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
                json!({"call_id": "call-1", "name": "shell", "arguments": {"cmd": "pwd"}}),
            ),
            event(
                3,
                "tool_result",
                json!({
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
                result: Some(EventSequence(3)),
            }]
        );
    }

    #[test]
    fn active_items_attach_to_the_last_turn_by_id() {
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
        let activity = activity(&state, &ActivityKey::Active(ActiveItemId(7))).unwrap();
        assert_eq!(activity.body, "streaming");
        assert!(activity.active);
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
            event(1, "compaction", json!({"items": [], "checkpoint_id": 1})),
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
            event(1, "compaction", json!({"items": [], "checkpoint_id": 1})),
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
}
