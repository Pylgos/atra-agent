use anyhow::Result;
use atra_protocol::{
    AssistantMessagePhase, CompactionReplacement, EventSequence, InstructionEvent,
    ModelRequestKind, OpaqueState, RunnersEvent, ThreadEventData, ToolCallEvent,
    ToolOutputForgetting, ToolResultEvent, project_tool_output_forgetting,
};
use serde_json::Value;

use super::{format_runners, format_skill_invocation};
use crate::storage::Event;

const FORGET_OUTPUT_HINT_TOKENS: usize = 500;
const FORGOTTEN_OUTPUT: &str = "[tool output forgotten]";

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Surface {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Item {
    Message {
        role: Role,
        text: String,
        phase: Option<AssistantMessagePhase>,
    },
    Reasoning {
        summary: String,
        opaque: Option<OpaqueState>,
    },
    ToolCall {
        kind: ToolKind,
        item_id: Option<String>,
        call_id: String,
        name: String,
        input: ToolInput,
    },
    ToolResult {
        kind: ToolKind,
        call_id: String,
        name: String,
        output: Value,
    },
    WebSearch(Value),
    Opaque(OpaqueState),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Developer,
    User,
    Assistant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolKind {
    Function,
    Custom,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ToolInput {
    Json(Value),
    Text(String),
}

pub(super) fn derive(
    events: &[Event],
    replay_key: Option<&str>,
    request_kind: ModelRequestKind,
) -> Result<Surface> {
    let mut items = Vec::new();
    let forgetting =
        project_tool_output_forgetting(events.iter().map(|event| (event.sequence, &event.data)));
    if let Some(context) = events.iter().find_map(|event| match &event.data {
        ThreadEventData::ThreadContext(context) => Some(context),
        _ => None,
    }) {
        items.push(message(Role::Developer, context.content.clone()));
    }

    let compaction = applicable_compaction(events, replay_key);
    let through = compaction.map(|event| event.through);
    if let Some(compaction) = compaction {
        match &compaction.replacement {
            CompactionReplacement::Summary { content } => items.push(message(
                Role::Developer,
                format!("Summary of the earlier conversation:\n\n{content}"),
            )),
            CompactionReplacement::Opaque { state } => items.push(Item::Opaque(state.clone())),
        }
        append_shadowed_state(&mut items, events, compaction.through);
    }

    for event in events {
        if through.is_some_and(|through| event.sequence <= through) {
            continue;
        }
        if matches!(
            &event.data,
            ThreadEventData::ModelRequest(request) if request.kind == ModelRequestKind::Response
        ) && let Some(hint) =
            forget_hint(forgetting.request_batch(event.sequence), events, through)
        {
            items.push(message(Role::Developer, hint));
        }
        let item = event_item(event, &forgetting);
        if let Some(item) = item {
            items.push(item);
        }
    }
    if request_kind == ModelRequestKind::Response
        && let Some(hint) = forget_hint(forgetting.current_batch(), events, through)
    {
        items.push(message(Role::Developer, hint));
    }
    Ok(Surface { items })
}

pub(crate) fn applicable_compaction<'a>(
    events: &'a [Event],
    replay_key: Option<&str>,
) -> Option<&'a atra_protocol::CompactionEvent> {
    events.iter().rev().find_map(|event| match &event.data {
        ThreadEventData::Compaction(compaction)
            if match &compaction.replacement {
                CompactionReplacement::Summary { .. } => true,
                CompactionReplacement::Opaque { state } => {
                    replay_key.is_some_and(|key| key == state.replay_key)
                }
            } =>
        {
            Some(compaction)
        }
        _ => None,
    })
}

fn append_shadowed_state(
    items: &mut Vec<Item>,
    events: &[Event],
    through: atra_protocol::EventSequence,
) {
    let visible = |event: &&Event| event.sequence > through;
    if !events
        .iter()
        .filter(visible)
        .any(|event| matches!(event.data, ThreadEventData::WorkspaceInstructions(_)))
        && let Some(value) = events.iter().rev().find_map(|event| match &event.data {
            ThreadEventData::WorkspaceInstructions(value) => Some(value),
            _ => None,
        })
    {
        items.push(workspace_instructions_item(value));
    }
    if !events
        .iter()
        .filter(visible)
        .any(|event| matches!(event.data, ThreadEventData::Skills(_)))
        && let Some(value) = events.iter().rev().find_map(|event| match &event.data {
            ThreadEventData::Skills(value) => Some(value),
            _ => None,
        })
    {
        items.push(message(
            Role::Developer,
            instruction_text(value, "skills list"),
        ));
    }
    if !events
        .iter()
        .filter(visible)
        .any(|event| matches!(event.data, ThreadEventData::Runners(_)))
        && let Some(runners) = events.iter().rev().find_map(|event| match &event.data {
            ThreadEventData::Runners(RunnersEvent::Initial(runners))
            | ThreadEventData::Runners(RunnersEvent::Replacement(runners)) => Some(runners),
            _ => None,
        })
    {
        items.push(message(Role::Developer, format_runners(runners)));
    }
}

fn event_item(event: &Event, forgetting: &ToolOutputForgetting) -> Option<Item> {
    match &event.data {
        ThreadEventData::ThreadContext(_) => None,
        ThreadEventData::WorkspaceInstructions(value) => Some(workspace_instructions_item(value)),
        ThreadEventData::Skills(value) => Some(message(
            Role::Developer,
            instruction_text(value, "skills list"),
        )),
        ThreadEventData::SkillInvocation(value) => {
            Some(message(Role::User, format_skill_invocation(value)))
        }
        ThreadEventData::Runners(value) => Some(message(
            Role::Developer,
            match value {
                RunnersEvent::Initial(runners) => format_runners(runners),
                RunnersEvent::Replacement(runners) => format!(
                    "The available Atra Runner list has changed. This list replaces the previously provided list.\n\n{}",
                    format_runners(runners)
                ),
            },
        )),
        ThreadEventData::UserMessage(value) => Some(message(Role::User, value.content.clone())),
        ThreadEventData::AssistantMessage(value) => Some(Item::Message {
            role: Role::Assistant,
            text: value.content.clone(),
            phase: Some(value.phase),
        }),
        ThreadEventData::Reasoning(value) => Some(Item::Reasoning {
            summary: value.summary.clone(),
            opaque: value.opaque.clone(),
        }),
        ThreadEventData::ToolCall(value) => Some(match value {
            ToolCallEvent::Custom {
                item_id,
                name,
                input,
                call_id,
            } => Item::ToolCall {
                kind: ToolKind::Custom,
                item_id: item_id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                input: ToolInput::Text(input.clone()),
            },
            ToolCallEvent::Function {
                name,
                arguments,
                call_id,
            } => Item::ToolCall {
                kind: ToolKind::Function,
                item_id: None,
                call_id: call_id.clone(),
                name: name.clone(),
                input: ToolInput::Json(arguments.clone()),
            },
        }),
        ThreadEventData::ToolResult(value) => {
            let (kind, call_id, name, result) = match value {
                ToolResultEvent::Custom {
                    call_id,
                    name,
                    result,
                    ..
                } => (ToolKind::Custom, call_id, name, result),
                ToolResultEvent::Function {
                    call_id,
                    name,
                    result,
                    ..
                } => (ToolKind::Function, call_id, name, result),
            };
            Some(Item::ToolResult {
                kind,
                call_id: call_id.clone(),
                name: name.clone(),
                output: if forgetting.summary(event.sequence).is_some() {
                    Value::String(FORGOTTEN_OUTPUT.to_owned())
                } else {
                    result.clone()
                },
            })
        }
        ThreadEventData::WebSearch(value) => Some(Item::WebSearch(value.item.clone())),
        ThreadEventData::Compaction(_)
        | ThreadEventData::ModelRequest(_)
        | ThreadEventData::TokenUsage(_)
        | ThreadEventData::RateLimits(_)
        | ThreadEventData::ApprovalDecision(_)
        | ThreadEventData::Retry(_)
        | ThreadEventData::TurnOutcome(_) => None,
    }
}

fn forget_hint(
    batch: &[EventSequence],
    events: &[Event],
    through: Option<EventSequence>,
) -> Option<String> {
    let mut rows = Vec::new();
    for (newest_index, sequence) in batch.iter().rev().enumerate() {
        if through.is_some_and(|through| *sequence <= through) {
            continue;
        }
        let Some(result) = events.iter().find_map(|event| {
            (event.sequence == *sequence).then(|| match &event.data {
                ThreadEventData::ToolResult(result) => Some(result),
                _ => None,
            })?
        }) else {
            continue;
        };
        let value = match result {
            ToolResultEvent::Custom {
                call_id, result, ..
            }
            | ToolResultEvent::Function {
                call_id, result, ..
            } => {
                if super::text_tokens(
                    &serde_json::to_string(result).expect("tool result is serializable"),
                ) < FORGET_OUTPUT_HINT_TOKENS
                {
                    continue;
                }
                (call_id, newest_index + 1)
            }
        };
        rows.push(format!("- {}: {}", recency_label(value.1), value.0));
    }
    (!rows.is_empty()).then(|| format!("Forgettable tool outputs:\n{}", rows.join("\n")))
}

fn recency_label(position: usize) -> String {
    match position {
        1 => "Most recent".to_owned(),
        position => format!("{} most recent", ordinal(position)),
    }
}

fn ordinal(value: usize) -> String {
    let suffix = if (11..=13).contains(&(value % 100)) {
        "th"
    } else {
        match value % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{value}{suffix}")
}

fn workspace_instructions_item(value: &InstructionEvent) -> Item {
    message(
        Role::Developer,
        format!(
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\n{}\n</INSTRUCTIONS>",
            instruction_text(value, "AGENTS.md instructions")
        ),
    )
}

fn message(role: Role, text: String) -> Item {
    Item::Message {
        role,
        text,
        phase: None,
    }
}

fn instruction_text(value: &InstructionEvent, removal: &str) -> String {
    match value {
        InstructionEvent::Initial(value) | InstructionEvent::Replacement(value) => value.clone(),
        InstructionEvent::Removal => format!("The {removal} were removed."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atra_protocol::{AssistantMessageEvent, EventSequence, MessageEvent, ModelRequestEvent};

    #[test]
    fn compaction_shadows_only_the_history_prefix() {
        let events = vec![
            Event {
                sequence: EventSequence(0),
                data: ThreadEventData::ThreadContext(MessageEvent {
                    content: "context".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(1),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "old".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(2),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "recent".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(3),
                data: ThreadEventData::Compaction(atra_protocol::CompactionEvent {
                    replacement: atra_protocol::CompactionReplacement::Summary {
                        content: "summary".to_owned(),
                    },
                    through: EventSequence(1),
                }),
            },
            Event {
                sequence: EventSequence(4),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "new".to_owned(),
                }),
            },
        ];

        assert_eq!(
            derive(&events, None, ModelRequestKind::Response)
                .unwrap()
                .items,
            vec![
                message(Role::Developer, "context".to_owned()),
                message(
                    Role::Developer,
                    "Summary of the earlier conversation:\n\nsummary".to_owned()
                ),
                message(Role::User, "recent".to_owned()),
                message(Role::User, "new".to_owned()),
            ]
        );
    }

    #[test]
    fn opaque_compaction_only_shadows_for_a_matching_replay_key() {
        let events = vec![
            Event {
                sequence: EventSequence(0),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "old".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(1),
                data: ThreadEventData::Compaction(atra_protocol::CompactionEvent {
                    replacement: atra_protocol::CompactionReplacement::Opaque {
                        state: OpaqueState {
                            replay_key: "codex/model/compaction-v1".to_owned(),
                            payload: serde_json::json!({"type": "compaction"}),
                        },
                    },
                    through: EventSequence(0),
                }),
            },
            Event {
                sequence: EventSequence(2),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "new".to_owned(),
                }),
            },
        ];

        assert_eq!(
            derive(
                &events,
                Some("codex/model/compaction-v1"),
                ModelRequestKind::Response
            )
            .unwrap()
            .items,
            vec![
                Item::Opaque(OpaqueState {
                    replay_key: "codex/model/compaction-v1".to_owned(),
                    payload: serde_json::json!({"type": "compaction"}),
                }),
                message(Role::User, "new".to_owned()),
            ]
        );
        assert_eq!(
            derive(
                &events,
                Some("other/model/compaction-v1"),
                ModelRequestKind::Response
            )
            .unwrap()
            .items,
            vec![
                message(Role::User, "old".to_owned()),
                message(Role::User, "new".to_owned()),
            ]
        );
    }

    #[test]
    fn latest_compaction_replaces_an_earlier_summary_and_keeps_its_suffix() {
        let events = vec![
            Event {
                sequence: EventSequence(0),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "old".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(1),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "first retained".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(2),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "second retained".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(3),
                data: ThreadEventData::Compaction(atra_protocol::CompactionEvent {
                    replacement: atra_protocol::CompactionReplacement::Summary {
                        content: "first summary".to_owned(),
                    },
                    through: EventSequence(0),
                }),
            },
            Event {
                sequence: EventSequence(4),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "new".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(5),
                data: ThreadEventData::Compaction(atra_protocol::CompactionEvent {
                    replacement: atra_protocol::CompactionReplacement::Summary {
                        content: "second summary".to_owned(),
                    },
                    through: EventSequence(1),
                }),
            },
        ];

        assert_eq!(
            derive(&events, None, ModelRequestKind::Response)
                .unwrap()
                .items,
            vec![
                message(
                    Role::Developer,
                    "Summary of the earlier conversation:\n\nsecond summary".to_owned()
                ),
                message(Role::User, "second retained".to_owned()),
                message(Role::User, "new".to_owned()),
            ]
        );
    }

    #[test]
    fn compaction_restores_shadowed_workspace_state() {
        let events = vec![
            Event {
                sequence: EventSequence(0),
                data: ThreadEventData::WorkspaceInstructions(InstructionEvent::Initial(
                    "instructions".to_owned(),
                )),
            },
            Event {
                sequence: EventSequence(1),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "old".to_owned(),
                }),
            },
            Event {
                sequence: EventSequence(2),
                data: ThreadEventData::Compaction(atra_protocol::CompactionEvent {
                    replacement: atra_protocol::CompactionReplacement::Summary {
                        content: "summary".to_owned(),
                    },
                    through: EventSequence(1),
                }),
            },
        ];

        assert_eq!(
            derive(&events, None, ModelRequestKind::Response)
                .unwrap()
                .items,
            vec![
                message(
                    Role::Developer,
                    "Summary of the earlier conversation:\n\nsummary".to_owned()
                ),
                workspace_instructions_item(&InstructionEvent::Initial("instructions".to_owned())),
            ]
        );
    }

    fn tool_result(sequence: i64, call_id: &str, result: Value) -> Event {
        Event {
            sequence: EventSequence(sequence),
            data: ThreadEventData::ToolResult(ToolResultEvent::Function {
                name: "tool".to_owned(),
                call_id: call_id.to_owned(),
                result,
                artifacts: Vec::new(),
            }),
        }
    }

    fn response_request(sequence: i64) -> Event {
        Event {
            sequence: EventSequence(sequence),
            data: ThreadEventData::ModelRequest(ModelRequestEvent {
                kind: ModelRequestKind::Response,
                context_window: None,
            }),
        }
    }

    #[test]
    fn forgetting_replaces_the_result_and_reconstructs_the_historical_hint() {
        let events = vec![
            tool_result(1, "small", serde_json::json!("small")),
            tool_result(2, "large", serde_json::json!("token ".repeat(700))),
            response_request(3),
            Event {
                sequence: EventSequence(4),
                data: ThreadEventData::AssistantMessage(AssistantMessageEvent {
                    content: "<forget_output call_id=\"large\">preserve this fact</forget_output>"
                        .to_owned(),
                    phase: AssistantMessagePhase::Commentary,
                    todos: Vec::new(),
                }),
            },
        ];

        let items = derive(&events, None, ModelRequestKind::Response)
            .unwrap()
            .items;

        assert!(matches!(
            &items[1],
            Item::ToolResult { output, .. }
                if output == &Value::String(FORGOTTEN_OUTPUT.to_owned())
        ));
        assert_eq!(
            items[2],
            message(
                Role::Developer,
                "Forgettable tool outputs:\n- Most recent: large".to_owned()
            )
        );
        assert!(matches!(
            &items[3],
            Item::Message {
                role: Role::Assistant,
                text,
                ..
            } if text.contains("preserve this fact")
        ));
    }

    #[test]
    fn current_hint_counts_small_outputs_but_is_suppressed_for_compaction() {
        let events = vec![
            response_request(1),
            tool_result(2, "large", serde_json::json!("token ".repeat(700))),
            tool_result(3, "small", serde_json::json!("small")),
        ];

        let response = derive(&events, None, ModelRequestKind::Response)
            .unwrap()
            .items;
        assert_eq!(
            response.last(),
            Some(&message(
                Role::Developer,
                "Forgettable tool outputs:\n- 2nd most recent: large".to_owned()
            ))
        );

        let compaction = derive(&events, None, ModelRequestKind::Compaction)
            .unwrap()
            .items;
        assert!(!compaction.iter().any(|item| {
            matches!(
                item,
                Item::Message {
                    role: Role::Developer,
                    text,
                    ..
                } if text.starts_with("Forgettable tool outputs:")
            )
        }));
    }
}
