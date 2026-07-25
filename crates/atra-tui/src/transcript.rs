use atra_protocol::ThreadEvent;
use ratatui::text::Line;

mod render;

pub(crate) use render::{
    layout_transcript, prepare_transcript, transcript_lines, transcript_ranges, transcript_text,
};

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
    ToolCall {
        name: String,
        arguments: Option<serde_json::Value>,
    },
    ToolResult {
        result: serde_json::Value,
    },
    Approval {
        id: u64,
        tool: Option<String>,
        allowed: Option<bool>,
    },
}

impl TranscriptItem {
    pub(crate) fn message(author: Author, text: String) -> Self {
        Self::Message { author, text }
    }

    pub(crate) fn append_message(&mut self, content: &str) {
        let Self::Message { text, .. } = self else {
            unreachable!()
        };
        text.push_str(content);
    }

    pub(crate) fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult { .. })
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
    pub(crate) rendered: Option<RenderedItem>,
}

impl TranscriptEntry {
    pub(crate) fn new(item: TranscriptItem) -> Self {
        Self {
            item,
            rendered: None,
        }
    }

    pub(crate) fn message(author: Author, text: String) -> Self {
        Self::new(TranscriptItem::message(author, text))
    }

    pub(crate) fn append_message(&mut self, content: &str) {
        self.item.append_message(content);
        self.rendered = None;
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
    match event.kind.as_str() {
        "user_message" => Some(TranscriptItem::message(
            Author::User,
            sanitize(event.payload.get("content")?.as_str()?),
        )),
        "assistant_message" => Some(TranscriptItem::message(
            Author::Assistant,
            sanitize(event.payload.get("content")?.as_str()?),
        )),
        "tool_call" => Some(TranscriptItem::ToolCall {
            name: sanitize(event.payload.get("name")?.as_str()?),
            arguments: Some(sanitize_value(
                event
                    .payload
                    .get("input")
                    .cloned()
                    .or_else(|| event.payload.get("arguments").cloned())?,
            )),
        }),
        "tool_result" => Some(TranscriptItem::ToolResult {
            result: sanitize_value(event.payload.get("result")?.clone()),
        }),
        "approval_request" => Some(TranscriptItem::Approval {
            id: event.payload.get("approval_id")?.as_u64()?,
            tool: Some(sanitize(event.payload.get("tool")?.as_str()?)),
            allowed: None,
        }),
        "approval_response" => Some(TranscriptItem::Approval {
            id: event.payload.get("approval_id")?.as_u64()?,
            tool: None,
            allowed: Some(event.payload.get("decision")?.as_str()? == "allow"),
        }),
        _ => None,
    }
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
            sequence: 1,
            kind: "tool_call".to_owned(),
            payload: serde_json::json!({
                "name": "exec\u{1b}[31m_command",
                "input": {
                    "command": "safe\u{1b}]52;c;bad\u{7}"
                }
            }),
        };

        let Some(TranscriptItem::ToolCall { name, arguments }) = item_from_event(event) else {
            panic!("tool call event was not converted");
        };
        assert_eq!(name, "exec_command");
        assert_eq!(arguments.unwrap()["command"], "safe");
    }
}
