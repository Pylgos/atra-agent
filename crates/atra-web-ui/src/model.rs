use atra_protocol::{
    CheckpointState, CheckpointSubscriptionMessage, ControllerState, ControllerSubscriptionMessage,
    ProcessState, ProcessSubscriptionMessage, ThreadEventData, ThreadState,
    ThreadSubscriptionMessage,
};
use pulldown_cmark::{CowStr, Event, Options, Parser, html};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub workspace_id: String,
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct WorkspaceList {
    pub workspaces: Vec<Workspace>,
}

#[derive(Clone, Debug)]
pub struct RemoteState<T> {
    pub value: Option<T>,
    pub connected: bool,
    pub terminal: Option<String>,
}

impl<T> Default for RemoteState<T> {
    fn default() -> Self {
        Self {
            value: None,
            connected: false,
            terminal: None,
        }
    }
}

macro_rules! apply_subscription {
    ($state:ty, $message:ident) => {
        impl RemoteState<$state> {
            pub fn apply(&mut self, message: $message) {
                match message {
                    $message::Snapshot { state } => {
                        self.value = Some(state);
                        self.connected = true;
                        self.terminal = None;
                    }
                    $message::Operation { operation } => {
                        let result: Result<(), String> = match self.value.as_mut() {
                            Some(state) => operation
                                .apply(state)
                                .map(|_| ())
                                .map_err(|error| error.to_string()),
                            None => Err("operation arrived before snapshot".to_owned()),
                        };
                        if let Err(error) = result {
                            self.connected = false;
                            self.terminal = Some(error);
                        }
                    }
                    $message::Terminal { terminal } => {
                        self.connected = false;
                        self.terminal = Some(format!("{terminal:?}"));
                    }
                }
            }
        }
    };
}

apply_subscription!(ControllerState, ControllerSubscriptionMessage);
apply_subscription!(ThreadState, ThreadSubscriptionMessage);
apply_subscription!(ProcessState, ProcessSubscriptionMessage);

impl RemoteState<CheckpointState> {
    pub fn apply(&mut self, message: CheckpointSubscriptionMessage) {
        match message {
            CheckpointSubscriptionMessage::Snapshot { state } => {
                self.value = Some(state);
                self.connected = true;
                self.terminal = None;
            }
            CheckpointSubscriptionMessage::Terminal { terminal } => {
                self.connected = false;
                self.terminal = Some(format!("{terminal:?}"));
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detail {
    Checkpoint(i64),
    Process { runner: String, process: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub workspace: Option<String>,
    pub thread: Option<i64>,
    pub detail: Option<Detail>,
}

impl Route {
    pub fn parse(path: &str) -> Self {
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["w", workspace, "threads", thread, "checkpoints", checkpoint] => Self {
                workspace: Some((*workspace).to_owned()),
                thread: thread.parse().ok(),
                detail: checkpoint.parse().ok().map(Detail::Checkpoint),
            },
            [
                "w",
                workspace,
                "threads",
                thread,
                "processes",
                runner,
                process,
            ] => Self {
                workspace: Some((*workspace).to_owned()),
                thread: thread.parse().ok(),
                detail: Some(Detail::Process {
                    runner: (*runner).to_owned(),
                    process: (*process).to_owned(),
                }),
            },
            ["w", workspace, "threads", thread] => Self {
                workspace: Some((*workspace).to_owned()),
                thread: thread.parse().ok(),
                detail: None,
            },
            ["w", workspace] => Self {
                workspace: Some((*workspace).to_owned()),
                thread: None,
                detail: None,
            },
            _ => Self {
                workspace: None,
                thread: None,
                detail: None,
            },
        }
    }

    pub fn path(&self) -> String {
        match (&self.workspace, self.thread, &self.detail) {
            (Some(workspace), Some(thread), Some(Detail::Checkpoint(checkpoint))) => {
                format!("/w/{workspace}/threads/{thread}/checkpoints/{checkpoint}")
            }
            (Some(workspace), Some(thread), Some(Detail::Process { runner, process })) => {
                format!("/w/{workspace}/threads/{thread}/processes/{runner}/{process}")
            }
            (Some(workspace), Some(thread), None) => format!("/w/{workspace}/threads/{thread}"),
            (Some(workspace), None, _) => format!("/w/{workspace}"),
            _ => "/".to_owned(),
        }
    }
}

pub fn draft_key(workspace: &str, thread: i64) -> String {
    format!("atra:draft:{workspace}:{thread}")
}

pub fn history_key(workspace: &str) -> String {
    format!("atra:sent-history:{workspace}")
}

pub const THEME_KEY: &str = "atra:theme";
pub const NOTIFICATIONS_KEY: &str = "atra:notifications";

#[derive(Clone, Debug, Serialize)]
pub struct TranscriptItem {
    pub label: String,
    pub body: String,
    pub active: bool,
    pub markdown: bool,
}

pub fn transcript(state: &ThreadState) -> Vec<TranscriptItem> {
    let mut items = state
        .events()
        .iter()
        .map(|event| {
            let (label, body, markdown) = match &event.data {
                ThreadEventData::UserMessage(message) => ("You", message.content.clone(), false),
                ThreadEventData::AssistantMessage(message) => {
                    ("Assistant", message.content.clone(), true)
                }
                ThreadEventData::Reasoning(item) => ("Reasoning", pretty(&item.item), false),
                ThreadEventData::ToolCall(call) => ("Tool call", pretty(call), false),
                ThreadEventData::ToolResult(result) => ("Tool result", pretty(result), false),
                ThreadEventData::WebSearch(item) => ("Web search", pretty(&item.item), false),
                ThreadEventData::Compaction(event) => ("Compaction", pretty(event), false),
                ThreadEventData::ModelRequest(event) => ("Model request", pretty(event), false),
                ThreadEventData::FrozenBoundary(event) => {
                    ("History boundary", pretty(event), false)
                }
                other => (other.kind(), pretty(other), false),
            };
            TranscriptItem {
                label: format!("{label} · {}", event.sequence.0),
                body,
                active: false,
                markdown,
            }
        })
        .collect::<Vec<_>>();
    if let Some(turn) = state.active_turn() {
        items.extend(turn.items().iter().map(|item| TranscriptItem {
            label: format!("Active · {:?}", turn.phase()),
            body: pretty(item.data()),
            active: true,
            markdown: false,
        }));
    }
    items
}

pub fn render_markdown(source: &str) -> String {
    let parser = Parser::new_ext(
        source,
        Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TABLES
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES,
    )
    .map(|event| match event {
        Event::Html(value) | Event::InlineHtml(value) => {
            Event::Text(CowStr::from(value.into_string()))
        }
        event => event,
    });
    let mut rendered = String::new();
    html::push_html(&mut rendered, parser);

    let tags = [
        "a",
        "blockquote",
        "br",
        "code",
        "del",
        "em",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "li",
        "ol",
        "p",
        "pre",
        "strong",
        "table",
        "tbody",
        "td",
        "th",
        "thead",
        "tr",
        "ul",
    ]
    .into_iter()
    .collect::<HashSet<_>>();
    let schemes = ["http", "https"].into_iter().collect::<HashSet<_>>();
    ammonia::Builder::new()
        .tags(tags)
        .url_schemes(schemes)
        .url_relative(ammonia::UrlRelative::Deny)
        .set_tag_attribute_value("a", "target", "_blank")
        .link_rel(Some("noopener noreferrer"))
        .clean(&rendered)
        .to_string()
}

fn pretty(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "Unable to render event".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_strict_and_round_trip() {
        for path in [
            "/w/abc/threads/42",
            "/w/abc/threads/42/checkpoints/7",
            "/w/abc/threads/42/processes/sandbox/process-1",
        ] {
            assert_eq!(Route::parse(path).path(), path);
        }
        assert_eq!(Route::parse("/other/abc").workspace, None);
    }

    #[test]
    fn browser_storage_is_scoped_without_migrations() {
        assert_eq!(draft_key("one", 2), "atra:draft:one:2");
        assert_eq!(history_key("one"), "atra:sent-history:one");
        assert_ne!(draft_key("one", 2), draft_key("two", 2));
    }

    #[test]
    fn markdown_uses_a_sanitized_standard_parser() {
        let rendered = render_markdown(
            "# Result\n<script>alert(1)</script>\n[good](https://example.com)\n[bad](javascript:alert(1))",
        );
        assert!(rendered.contains("<h1>Result</h1>"));
        assert!(rendered.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(rendered.contains("href=\"https://example.com\""));
        assert!(rendered.contains("rel=\"noopener noreferrer\""));
        assert!(!rendered.contains("href=\"javascript:"));
    }
}
