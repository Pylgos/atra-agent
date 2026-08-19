use atra_protocol::{
    AgentStatus, ControllerState, ControllerSubscriptionMessage, ProcessState,
    ProcessSubscriptionMessage, Thread, ThreadEventData, ThreadId, ThreadState,
};
use pulldown_cmark::{CowStr, Event, Options, Parser, html};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};

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
                        let result = self
                            .value
                            .as_mut()
                            .ok_or_else(|| "operation arrived before snapshot".to_owned())
                            .and_then(|state| {
                                operation
                                    .apply(state)
                                    .map(|_| ())
                                    .map_err(|e| e.to_string())
                            });
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
apply_subscription!(ProcessState, ProcessSubscriptionMessage);

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
        let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptMode {
    Pretty,
    Raw,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UtilityTab {
    Thread,
    Activity,
    Children,
    Checkpoints,
    Processes,
}

impl UtilityTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Thread => "Thread",
            Self::Activity => "Activity",
            Self::Children => "Children",
            Self::Checkpoints => "Checkpoints",
            Self::Processes => "Processes",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Pin {
    pub workspace: String,
    pub thread: i64,
}

pub const THEME_KEY: &str = "atra:theme";
pub const NOTIFICATIONS_KEY: &str = "atra:notifications";
pub const PINS_KEY: &str = "atra:pins";
pub const LAST_ROUTE_KEY: &str = "atra:last-route";
pub const NAV_OPEN_KEY: &str = "atra:navigation-open";
pub const UTILITY_OPEN_KEY: &str = "atra:utility-open";
pub const UTILITY_TAB_KEY: &str = "atra:utility-tab";
pub const NAV_WIDTH_KEY: &str = "atra:navigation-width";
pub const UTILITY_WIDTH_KEY: &str = "atra:utility-width";
pub const WORKSPACE_COLLAPSE_KEY: &str = "atra:workspace-collapse";
pub const SENT_HISTORY_KEY: &str = "atra:sent-history";

pub fn draft_key(workspace: &str, thread: i64) -> String {
    format!("atra:draft:{workspace}:{thread}")
}

pub fn thread_name(thread: &Thread) -> String {
    thread
        .display_name
        .clone()
        .unwrap_or_else(|| format!("Thread {}", thread.id.0))
}

pub fn root_id(threads: &[Thread], thread_id: ThreadId) -> ThreadId {
    let by_id = threads
        .iter()
        .map(|thread| (thread.id, thread))
        .collect::<HashMap<_, _>>();
    let mut current = thread_id;
    while let Some(parent) = by_id
        .get(&current)
        .and_then(|thread| thread.parent_thread_id)
    {
        current = parent;
    }
    current
}

pub fn root_threads(controller: &ControllerState) -> Vec<Thread> {
    let mut roots = controller
        .threads()
        .iter()
        .filter(|thread| thread.parent_thread_id.is_none())
        .cloned()
        .collect::<Vec<_>>();
    roots.sort_by_key(|thread| std::cmp::Reverse(thread.id));
    roots
}

pub fn family_threads(controller: &ControllerState, selected: ThreadId) -> Vec<Thread> {
    let root = root_id(controller.threads(), selected);
    let mut family = controller
        .threads()
        .iter()
        .filter(|thread| root_id(controller.threads(), thread.id) == root)
        .cloned()
        .collect::<Vec<_>>();
    family.sort_by_key(|thread| {
        (
            thread.parent_thread_id.is_some(),
            std::cmp::Reverse(thread.id),
        )
    });
    family
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub token_summary: Option<String>,
    pub context_summary: Option<String>,
    pub cache_summary: Option<String>,
    pub composer_context: String,
    pub composer_cache: String,
    pub composer_quotas: Vec<String>,
    pub quota_windows: Vec<String>,
    pub usage_raw: Option<String>,
    pub limits_raw: Option<String>,
}

pub fn latest_diagnostics(state: &ThreadState) -> Diagnostics {
    let usage_event = state
        .events()
        .iter()
        .rev()
        .find_map(|event| match &event.data {
            ThreadEventData::TokenUsage(value) => Some(value),
            _ => None,
        });
    let context_window = usage_event.and_then(|usage| {
        state.events().iter().find_map(|event| {
            (event.sequence == usage.request_sequence)
                .then_some(&event.data)
                .and_then(|data| match data {
                    ThreadEventData::ModelRequest(request) => request.context_window,
                    _ => None,
                })
        })
    });
    let limits = state
        .events()
        .iter()
        .rev()
        .find_map(|event| match &event.data {
            ThreadEventData::RateLimits(value) => Some(&value.snapshots),
            _ => None,
        });
    diagnostics_from_values(
        usage_event.map(|event| &event.usage),
        context_window,
        limits,
    )
}

fn diagnostics_from_values(
    usage: Option<&Value>,
    context_window: Option<i64>,
    limits: Option<&Value>,
) -> Diagnostics {
    let input = usage.and_then(|usage| integer(&usage["input_tokens"]));
    let output = usage.and_then(|usage| integer(&usage["output_tokens"]));
    let total = usage
        .and_then(|usage| integer(&usage["total_tokens"]))
        .or_else(|| Some(input? + output?));
    let cached = usage.and_then(|usage| integer(&usage["cached_input_tokens"]));
    let token_summary = total.or(input).or(output).map(|_| {
        let mut parts = Vec::new();
        if let Some(total) = total {
            parts.push(format!("Tokens {}", format_integer(total)));
        }
        if let Some(input) = input {
            parts.push(format!("in {}", format_integer(input)));
        }
        if let Some(output) = output {
            parts.push(format!("out {}", format_integer(output)));
        }
        parts.join(" · ")
    });
    let context_summary =
        input
            .zip(context_window.filter(|window| *window > 0))
            .map(|(input, window)| {
                format!(
                    "Context {} / {} ({:.1}%)",
                    format_integer(input),
                    format_integer(window),
                    input as f64 / window as f64 * 100.0,
                )
            });
    let cache_summary = cached
        .zip(input.filter(|input| *input > 0))
        .map(|(cached, input)| {
            format!(
                "Cache {} / {} ({:.1}%)",
                format_integer(cached),
                format_integer(input),
                cached as f64 / input as f64 * 100.0,
            )
        });
    let composer_context = input
        .zip(context_window.filter(|window| *window > 0))
        .map(|(input, window)| format!("context {:.0}%", input as f64 / window as f64 * 100.0))
        .unwrap_or_else(|| "context —".to_owned());
    let composer_cache = input
        .filter(|input| *input > 0)
        .map(|input| {
            format!(
                "cache {:.0}%",
                cached.unwrap_or_default() as f64 / input as f64 * 100.0
            )
        })
        .unwrap_or_else(|| "cache —".to_owned());
    Diagnostics {
        token_summary,
        context_summary,
        cache_summary,
        composer_context,
        composer_cache,
        composer_quotas: limits.map(format_composer_quotas).unwrap_or_default(),
        quota_windows: limits.map(format_quota_windows).unwrap_or_default(),
        usage_raw: usage.map(pretty),
        limits_raw: limits.map(pretty),
    }
}

fn format_composer_quotas(value: &Value) -> Vec<String> {
    let snapshot = value
        .as_array()
        .and_then(|snapshots| {
            snapshots
                .iter()
                .rev()
                .find(|snapshot| snapshot["limit_id"] == "codex")
                .or_else(|| snapshots.last())
        })
        .or_else(|| value.is_object().then_some(value));
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|name| {
            let window = snapshot.get(name)?;
            let used = window["used_percent"].as_f64()?;
            let label = integer(&window["window_minutes"])
                .map(format_minutes)
                .unwrap_or_else(|| name.to_owned());
            Some(format!(
                "{label} {}",
                format_percent((100.0 - used).clamp(0.0, 100.0))
            ))
        })
        .collect()
}

fn format_quota_windows(value: &Value) -> Vec<String> {
    let snapshots = value
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(value));
    let multiple = snapshots.len() > 1;
    let now = unix_timestamp();
    let mut formatted = Vec::new();
    for snapshot in snapshots {
        let limit_name = snapshot["limit_name"]
            .as_str()
            .or_else(|| snapshot["limit_id"].as_str())
            .filter(|name| *name != "codex");
        for key in ["primary", "secondary"] {
            let window = &snapshot[key];
            let Some(used) = window["used_percent"].as_f64() else {
                continue;
            };
            let duration = integer(&window["window_minutes"])
                .map(format_minutes)
                .unwrap_or_else(|| key.to_owned());
            let prefix = if multiple {
                limit_name
                    .map(|name| format!("{name} "))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let remaining = (100.0 - used).clamp(0.0, 100.0);
            let reset = integer(&window["resets_at"])
                .map(|timestamp| format!(" · resets {}", format_seconds((timestamp - now).max(0))));
            formatted.push(format!(
                "{prefix}{duration} quota {} left{}",
                format_percent(remaining),
                reset.unwrap_or_default(),
            ));
        }
        if let Some(balance) = snapshot.pointer("/credits/balance").and_then(Value::as_f64) {
            formatted.push(format!("Credits {balance:.2}"));
        } else if snapshot
            .pointer("/credits/unlimited")
            .and_then(Value::as_bool)
            == Some(true)
        {
            formatted.push("Credits unlimited".to_owned());
        }
    }
    formatted
}

#[cfg(target_arch = "wasm32")]
fn unix_timestamp() -> i64 {
    (js_sys::Date::now() / 1000.0) as i64
}

#[cfg(not(target_arch = "wasm32"))]
fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_u64()?.try_into().ok())
}

fn format_integer(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(character);
    }
    if negative {
        formatted.insert(0, '-');
    }
    formatted
}

fn format_percent(value: f64) -> String {
    if value.fract().abs() < 0.05 {
        format!("{value:.0}%")
    } else {
        format!("{value:.1}%")
    }
}

fn format_minutes(minutes: i64) -> String {
    if minutes == 7 * 24 * 60 {
        "weekly".to_owned()
    } else if minutes >= 24 * 60 && minutes % (24 * 60) == 0 {
        format!("{}d", minutes / (24 * 60))
    } else if minutes >= 60 && minutes % 60 == 0 {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

fn format_seconds(seconds: i64) -> String {
    if seconds >= 24 * 60 * 60 {
        format!("in {}d", (seconds + 43_199) / (24 * 60 * 60))
    } else if seconds >= 60 * 60 {
        format!("in {}h", (seconds + 1_799) / (60 * 60))
    } else {
        format!("in {}m", ((seconds + 59) / 60).max(1))
    }
}

pub fn factual_status(status: Option<AgentStatus>) -> &'static str {
    match status {
        Some(AgentStatus::Idle) => "Idle",
        Some(AgentStatus::Running) => "Running",
        Some(AgentStatus::Compacting) => "Compacting",
        Some(AgentStatus::AwaitingQuestion) => "Question required",
        Some(AgentStatus::AwaitingApproval) => "Approval required",
        Some(AgentStatus::Cancelling) => "Cancelling",
        Some(AgentStatus::Completed) => "Completed",
        Some(AgentStatus::Failed) => "Failed",
        Some(AgentStatus::Cancelled) => "Cancelled",
        None => "Unknown",
    }
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
        "input",
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
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_tag_attributes("code", ["class"])
        .url_schemes(schemes)
        .url_relative(ammonia::UrlRelative::Deny)
        .set_tag_attribute_value("a", "target", "_blank")
        .link_rel(Some("noopener noreferrer"))
        .clean(&rendered)
        .to_string()
}

pub fn pretty(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "Unable to render value".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atra_protocol::ThreadEvent;
    use serde_json::json;

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
    fn browser_storage_scopes_drafts_per_thread_and_history_globally() {
        assert_eq!(draft_key("one", 2), "atra:draft:one:2");
        assert_ne!(draft_key("one", 2), draft_key("two", 2));
        assert_ne!(draft_key("one", 2), draft_key("one", 3));
        assert_eq!(SENT_HISTORY_KEY, "atra:sent-history");
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

    #[test]
    fn markdown_preserves_list_structure_and_safe_task_checkboxes() {
        let rendered = render_markdown("- one\n  - two\n- [x] done\n1. first");
        assert!(rendered.contains("<ul>"));
        assert!(rendered.contains("<ol>"));
        assert!(rendered.contains("<li>"));
        assert!(rendered.contains("type=\"checkbox\""));
        assert!(rendered.contains("disabled"));
    }

    #[test]
    fn markdown_code_block_keeps_language_class() {
        let rendered = render_markdown("```bash\necho hello\n```");
        assert!(rendered.contains("language-bash"), "rendered: {rendered}");
    }

    #[test]
    fn diagnostics_show_actual_usage_and_quota_values() {
        let usage = json!({
            "input_tokens": 6567,
            "cached_input_tokens": 3456,
            "output_tokens": 17,
            "total_tokens": 6584
        });
        let limits = json!([{
            "limit_id": "codex",
            "primary": {
                "used_percent": 42.0,
                "window_minutes": 300,
                "resets_at": 2_000_000_000
            },
            "secondary": {
                "used_percent": 20.0,
                "window_minutes": 10_080,
                "resets_at": 2_000_000_000
            }
        }]);
        let diagnostics = diagnostics_from_values(Some(&usage), Some(128_000), Some(&limits));

        assert_eq!(
            diagnostics.token_summary.as_deref(),
            Some("Tokens 6,584 · in 6,567 · out 17")
        );
        assert_eq!(
            diagnostics.context_summary.as_deref(),
            Some("Context 6,567 / 128,000 (5.1%)")
        );
        assert_eq!(
            diagnostics.cache_summary.as_deref(),
            Some("Cache 3,456 / 6,567 (52.6%)")
        );
        assert_eq!(diagnostics.composer_context, "context 5%");
        assert_eq!(diagnostics.composer_cache, "cache 53%");
        assert_eq!(diagnostics.composer_quotas, ["5h 58%", "weekly 80%"]);
        assert!(diagnostics.quota_windows[0].starts_with("5h quota 58% left"));
        assert!(diagnostics.quota_windows[1].starts_with("weekly quota 80% left"));
    }

    #[test]
    fn diagnostic_events_use_the_wire_shape_expected_by_the_web_client() {
        for value in [
            json!({
                "sequence": 74,
                "kind": "token_usage",
                "payload": {
                    "request_sequence": 71,
                    "usage": {
                        "input_tokens": 6567,
                        "cached_input_tokens": 3456,
                        "output_tokens": 17,
                        "total_tokens": 6584
                    }
                }
            }),
            json!({
                "sequence": 75,
                "kind": "rate_limits",
                "payload": {
                    "request_sequence": 71,
                    "snapshots": [{
                        "limit_id": "codex",
                        "primary": {
                            "used_percent": 42,
                            "window_minutes": 300,
                            "resets_at": 2_000_000_000_i64
                        }
                    }]
                }
            }),
        ] {
            serde_json::from_value::<ThreadEvent>(value).unwrap();
        }
    }

    #[test]
    fn thread_state_with_diagnostics_deserializes() {
        let state = json!({
            "metadata": {
                "id": 1,
                "parent_thread_id": null,
                "display_name": "Web Thread",
                "provider": "provider",
                "model": "model",
                "reasoning_effort": "medium"
            },
            "events": [
                {
                    "sequence": 1,
                    "kind": "user_message",
                    "payload": {"content": "prompt"}
                },
                {
                    "sequence": 2,
                    "kind": "assistant_message",
                    "payload": {"content": "answer", "phase": "final_answer"}
                },
                {
                    "sequence": 3,
                    "kind": "token_usage",
                    "payload": {
                        "request_sequence": 1,
                        "usage": {
                            "input_tokens": 6567,
                            "cached_input_tokens": 3456,
                            "output_tokens": 17,
                            "total_tokens": 6584
                        }
                    }
                },
                {
                    "sequence": 4,
                    "kind": "rate_limits",
                    "payload": {
                        "request_sequence": 1,
                        "snapshots": [{
                            "limit_id": "codex",
                            "primary": {
                                "used_percent": 42,
                                "window_minutes": 300,
                                "resets_at": 2_000_000_000_i64
                            }
                        }]
                    }
                }
            ],
            "active_turn": null,
            "last_outcome": null,
            "checkpoints": [],
            "processes": []
        });
        serde_json::from_value::<ThreadState>(state).unwrap();
    }
}
