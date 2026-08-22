use anyhow::Result;
use atra_protocol::{
    AssistantMessagePhase, CompactionReplacement, InstructionEvent, OpaqueState, RunnersEvent,
    ThreadEventData, ToolCallEvent, ToolResultEvent,
};
use serde_json::Value;

use super::{format_runners, format_skill_invocation};
use crate::storage::Event;

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

pub(super) fn derive(events: &[Event], replay_key: Option<&str>) -> Result<Surface> {
    let mut items = Vec::new();
    if let Some(context) = events.iter().find_map(|event| match &event.data {
        ThreadEventData::ThreadContext(context) => Some(context),
        _ => None,
    }) {
        items.push(message(Role::Developer, context.content.clone()));
    }

    let compaction = events.iter().rposition(|event| match &event.data {
        ThreadEventData::Compaction(event) => match &event.replacement {
            CompactionReplacement::Summary { .. } => true,
            CompactionReplacement::Opaque { state } => {
                replay_key.is_some_and(|key| key == state.replay_key)
            }
        },
        _ => false,
    });
    if let Some(index) = compaction
        && let ThreadEventData::Compaction(compaction) = &events[index].data
    {
        match &compaction.replacement {
            CompactionReplacement::Summary { content } => items.push(message(
                Role::Developer,
                format!("Summary of the earlier conversation:\n\n{content}"),
            )),
            CompactionReplacement::Opaque { state } => items.push(Item::Opaque(state.clone())),
        }
    }
    let events = compaction.map_or(events, |index| &events[index + 1..]);
    for event in events {
        let item = match &event.data {
            ThreadEventData::ThreadContext(_) => None,
            ThreadEventData::WorkspaceInstructions(value) => Some(message(
                Role::Developer,
                format!(
                    "# AGENTS.md instructions\n\n<INSTRUCTIONS>\n{}\n</INSTRUCTIONS>",
                    instruction_text(value, "AGENTS.md instructions")
                ),
            )),
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
                    output: result.clone(),
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
        };
        if let Some(item) = item {
            items.push(item);
        }
    }
    Ok(Surface { items })
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
    use atra_protocol::{EventSequence, MessageEvent};

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
                data: ThreadEventData::Compaction(atra_protocol::CompactionEvent {
                    replacement: atra_protocol::CompactionReplacement::Summary {
                        content: "summary".to_owned(),
                    },
                    checkpoint_id: atra_protocol::CheckpointId(1),
                }),
            },
            Event {
                sequence: EventSequence(3),
                data: ThreadEventData::UserMessage(MessageEvent {
                    content: "new".to_owned(),
                }),
            },
        ];

        assert_eq!(
            derive(&events, None).unwrap().items,
            vec![
                message(Role::Developer, "context".to_owned()),
                message(
                    Role::Developer,
                    "Summary of the earlier conversation:\n\nsummary".to_owned()
                ),
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
                    checkpoint_id: atra_protocol::CheckpointId(1),
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
            derive(&events, Some("codex/model/compaction-v1"))
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
            derive(&events, Some("other/model/compaction-v1"))
                .unwrap()
                .items,
            vec![
                message(Role::User, "old".to_owned()),
                message(Role::User, "new".to_owned()),
            ]
        );
    }
}
