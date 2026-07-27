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
    ToolResult {
        artifacts: Vec<ToolArtifact>,
        masked: bool,
    },
    Compaction,
}

#[derive(Clone)]
pub(crate) struct ToolArtifact {
    pub(crate) kind: String,
    pub(crate) data: serde_json::Value,
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
    pub(crate) sequence: Option<i64>,
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
        let TranscriptItem::ToolCall { arguments, .. } = &mut self.item else {
            unreachable!();
        };
        match arguments {
            Some(serde_json::Value::String(input)) => input.push_str(content),
            None => *arguments = Some(serde_json::Value::String(content.to_owned())),
            Some(_) => unreachable!(),
        }
        self.rendered = None;
    }

    pub(crate) fn replace(&mut self, item: TranscriptItem) {
        self.item = item;
        self.rendered = None;
    }

    pub(crate) fn replace_event(&mut self, sequence: i64, item: TranscriptItem) {
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
    match event.kind.as_str() {
        "user_message" => Some(TranscriptItem::message(
            Author::User,
            sanitize(event.payload.get("content")?.as_str()?),
        )),
        "assistant_message" => Some(TranscriptItem::message(
            Author::Assistant,
            sanitize(event.payload.get("content")?.as_str()?),
        )),
        "reasoning" => {
            let summary = event.payload.pointer("/item/summary")?.as_array()?;
            let text = summary
                .iter()
                .filter_map(|part| part.get("text")?.as_str())
                .map(sanitize)
                .collect::<Vec<_>>()
                .join("\n\n");
            (!text.is_empty()).then_some(TranscriptItem::ReasoningSummary { text })
        }
        "web_search" => Some(TranscriptItem::WebSearch {
            action: sanitize_value(
                event
                    .payload
                    .pointer("/item/action")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            ),
        }),
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
        "tool_result" => {
            let masked = event
                .payload
                .get("masked_result")
                .is_some_and(|masked| Some(masked) != event.payload.get("result"));
            Some(TranscriptItem::ToolResult {
                artifacts: event
                    .payload
                    .get("artifacts")?
                    .as_array()?
                    .iter()
                    .filter_map(|artifact| {
                        Some(ToolArtifact {
                            kind: sanitize(artifact.get("kind")?.as_str()?),
                            data: sanitize_value(artifact.get("data")?.clone()),
                        })
                    })
                    .collect(),
                masked,
            })
        }
        "compaction" => Some(TranscriptItem::Compaction),
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
