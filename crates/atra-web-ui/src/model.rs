use atra_protocol::{
    ControllerState, ControllerSubscriptionMessage, ThreadState, ThreadSubscriptionMessage,
};
use serde::{Deserialize, Serialize};

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
}

impl<T> Default for RemoteState<T> {
    fn default() -> Self {
        Self { value: None, connected: false }
    }
}

impl RemoteState<ControllerState> {
    pub fn apply(&mut self, message: ControllerSubscriptionMessage) {
        match message {
            ControllerSubscriptionMessage::Snapshot { state } => {
                self.value = Some(state);
                self.connected = true;
            }
            ControllerSubscriptionMessage::Operation { operation } => {
                let result = self
                    .value
                    .as_mut()
                    .ok_or("operation arrived before snapshot")
                    .and_then(|state| operation.apply(state).map(|_| ()).map_err(|_| "invalid operation"));
                if let Err(error) = result {
                    self.connected = false;
                    let _ = error;
                }
            }
            ControllerSubscriptionMessage::Terminal { terminal } => {
                self.connected = false;
                let _ = terminal;
            }
        }
    }
}

impl RemoteState<ThreadState> {
    pub fn apply(&mut self, message: ThreadSubscriptionMessage) {
        match message {
            ThreadSubscriptionMessage::Snapshot { state } => {
                self.value = Some(state);
                self.connected = true;
            }
            ThreadSubscriptionMessage::Operation { operation } => {
                let result = self
                    .value
                    .as_mut()
                    .ok_or("operation arrived before snapshot")
                    .and_then(|state| operation.apply(state).map(|_| ()).map_err(|_| "invalid operation"));
                if let Err(error) = result {
                    self.connected = false;
                    let _ = error;
                }
            }
            ThreadSubscriptionMessage::Terminal { terminal } => {
                self.connected = false;
                let _ = terminal;
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub workspace: Option<String>,
    pub thread: Option<i64>,
}

impl Route {
    pub fn parse(path: &str) -> Self {
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["w", workspace, "threads", thread] => Self {
                workspace: Some((*workspace).to_owned()),
                thread: thread.parse().ok(),
            },
            ["w", workspace] => Self {
                workspace: Some((*workspace).to_owned()),
                thread: None,
            },
            _ => Self { workspace: None, thread: None },
        }
    }

    pub fn path(&self) -> String {
        match (&self.workspace, self.thread) {
            (Some(workspace), Some(thread)) => format!("/w/{workspace}/threads/{thread}"),
            (Some(workspace), None) => format!("/w/{workspace}"),
            _ => "/".to_owned(),
        }
    }
}

pub fn draft_key(workspace: &str, thread: i64) -> String {
    format!("atra:draft:{workspace}:{thread}")
}



#[derive(Clone, Debug, Serialize)]
pub struct TranscriptItem {
    pub label: String,
    pub body: String,
    pub active: bool,
}

pub fn transcript(state: &ThreadState) -> Vec<TranscriptItem> {
    let mut items = state
        .events()
        .iter()
        .map(|event| {
            let value = serde_json::to_value(event).expect("ThreadEvent serialization is infallible");
            TranscriptItem {
                label: value.get("data")
                    .and_then(|data| data.get("kind"))
                    .and_then(|kind| kind.as_str())
                    .unwrap_or("event")
                    .to_owned(),
                body: serde_json::to_string_pretty(&value).unwrap(),
                active: false,
            }
        })
        .collect::<Vec<_>>();
    if let Some(turn) = state.active_turn() {
        items.extend(turn.items().iter().map(|item| TranscriptItem {
            label: "active".to_owned(),
            body: serde_json::to_string_pretty(item).unwrap(),
            active: true,
        }));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_strict_and_round_trip() {
        let route = Route::parse("/w/abc/threads/42");
        assert_eq!(route, Route { workspace: Some("abc".into()), thread: Some(42) });
        assert_eq!(route.path(), "/w/abc/threads/42");
        assert_eq!(Route::parse("/other/abc").workspace, None);
    }

    #[test]
    fn browser_storage_is_scoped_without_migrations() {
        assert_eq!(draft_key("one", 2), "atra:draft:one:2");
        assert_ne!(draft_key("one", 2), draft_key("two", 2));
    }
}
