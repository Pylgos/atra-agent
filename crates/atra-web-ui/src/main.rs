mod model;

use std::{cell::RefCell, rc::Rc};

use atra_protocol::{
    Command, CommandResult, ControllerState, ControllerSubscriptionMessage, ThreadId, ThreadState,
    ThreadSubscriptionMessage,
};
use dioxus::prelude::*;
use gloo_net::http::Request;
use model::{RemoteState, Route, WorkspaceList, draft_key, transcript};
use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::{Event, EventSource, MessageEvent};

fn main() {
    dioxus::launch(App);
}

struct SseConnection {
    source: EventSource,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _open: Closure<dyn FnMut(Event)>,
    _error: Closure<dyn FnMut(Event)>,
}

impl Drop for SseConnection {
    fn drop(&mut self) {
        self.source.close();
    }
}

fn connect_sse(
    url: &str,
    mut message: impl FnMut(String) + 'static,
    status: impl FnMut(bool) + 'static,
) -> Option<Rc<SseConnection>> {
    let source = EventSource::new(url).ok()?;
    let on_message = Closure::wrap(Box::new(move |event: MessageEvent| {
        if let Some(data) = event.data().as_string() {
            message(data);
        }
    }) as Box<dyn FnMut(MessageEvent)>);
    source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    let status = Rc::new(RefCell::new(status));
    let open_status = Rc::clone(&status);
    let on_open = Closure::wrap(Box::new(move |_event: Event| (open_status.borrow_mut())(true)) as Box<dyn FnMut(Event)>);
    source.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    let on_error = Closure::wrap(Box::new(move |_event: Event| (status.borrow_mut())(false)) as Box<dyn FnMut(Event)>);
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    Some(Rc::new(SseConnection { source, _message: on_message, _open: on_open, _error: on_error }))
}


struct PopstateListener {
    _callback: Closure<dyn FnMut(Event)>,
}

impl PopstateListener {
    fn new(mut route: Signal<Route>) -> Self {
        let callback = Closure::wrap(Box::new(move |_event: Event| route.set(browser_route()))
            as Box<dyn FnMut(Event)>);
        if let Some(window) = web_sys::window() {
            window.set_onpopstate(Some(callback.as_ref().unchecked_ref()));
        }
        Self { _callback: callback }
    }
}

impl Drop for PopstateListener {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            window.set_onpopstate(None);
        }
    }
}

fn browser_route() -> Route {
    web_sys::window()
        .and_then(|window| window.location().pathname().ok())
        .map(|path| Route::parse(&path))
        .unwrap_or_else(|| Route::parse("/"))
}

fn navigate(mut route: Signal<Route>, next: Route) {
    if let Some(window) = web_sys::window() {
        let _ = window.history().and_then(|history| {
            history.push_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&next.path()))
        });
    }
    route.set(next);
}

fn storage_get(key: &str) -> String {
    web_sys::window()
        .and_then(|window| window.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(key).ok().flatten())
        .unwrap_or_default()
}

fn storage_set(key: &str, value: &str) {
    if let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten()) {
        let _ = if value.is_empty() { storage.remove_item(key) } else { storage.set_item(key, value) };
    }
}

async fn command(workspace: &str, command: &Command) -> Result<CommandResult, String> {
    let response = Request::post(&format!("/api/workspaces/{workspace}/commands"))
        .header("Content-Type", "application/json")
        .json(command).map_err(|error| error.to_string())?
        .send().await.map_err(|error| error.to_string())?;
    if response.ok() {
        response.json().await.map_err(|error| error.to_string())
    } else {
        let value: serde_json::Value = response.json().await.map_err(|error| error.to_string())?;
        Err(value.get("error").and_then(|message| message.as_str()).unwrap_or("command failed").to_owned())
    }
}

#[component]
fn App() -> Element {
    let route = use_signal(browser_route);
    let _popstate = use_hook(move || Rc::new(PopstateListener::new(route)));
    let mut workspaces = use_signal(WorkspaceList::default);
    let mut daemon_connected = use_signal(|| false);
    let _workspace_stream = use_hook(move || {
        connect_sse(
            "/api/workspaces/events",
            move |data| {
                if let Ok(value) = serde_json::from_str(&data) {
                    workspaces.set(value);
                }
            },
            move |connected| daemon_connected.set(connected),
        )
    });
    let current = route.read().clone();

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/app.css") }
        main { class: "shell",
            aside { class: "navigation",
                h1 { "Atra" }
                p { "Web Client " span { class: "status", "experimental" } }
                p { class: "connection", if daemon_connected() { "Daemon connected" } else { "Daemon reconnecting…" } }
                nav { aria_label: "Workspaces",
                    if workspaces.read().workspaces.is_empty() {
                        p { "No running Workspaces." }
                    }
                    for workspace in workspaces.read().workspaces.clone() {
                        button {
                            class: "item",
                            onclick: move |_| navigate(route, Route { workspace: Some(workspace.workspace_id.clone()), thread: None }),
                            strong { "{workspace.name}" }
                            br {}
                            small { "{workspace.path}" }
                        }
                    }
                }
            }
            if let Some(workspace) = current.workspace {
                ControllerView { key: "{workspace}", workspace, selected_thread: current.thread, route }
            } else {
                section { class: "conversation empty",
                    h2 { "Choose a Workspace" }
                    p { "Start a Workspace Controller with atra, then select it here." }
                }
            }
        }
    }
}

#[component]
fn ControllerView(
    workspace: String,
    selected_thread: Option<i64>,
    route: Signal<Route>,
) -> Element {
    let mut remote = use_signal(RemoteState::<ControllerState>::default);
    let mut error = use_signal(String::new);
    let workspace_for_stream = workspace.clone();
    let _controller_stream = use_hook(move || {
        connect_sse(
            &format!("/api/workspaces/{workspace_for_stream}/controller/events"),
            move |data| match serde_json::from_str::<ControllerSubscriptionMessage>(&data) {
                Ok(message) => remote.write().apply(message),
                Err(parse_error) => error.set(parse_error.to_string()),
            },
            move |connected| {
                if !connected { remote.write().connected = false; }
            },
        )
    });
    let controller = remote.read().value.clone();
    let workspace_for_create = workspace.clone();

    rsx! {
        section { class: "conversation",
            header {
                strong { if remote.read().connected { "Connected" } else { "Read only — reconnecting…" } }
                if !error().is_empty() { p { role: "alert", "{error}" } }
            }
            nav { class: "thread-list", aria_label: "Threads",
                button {
                    disabled: !remote.read().connected,
                    onclick: move |_| {
                        let workspace = workspace_for_create.clone();
                        spawn(async move {
                            match command(&workspace, &Command::ThreadCreate { display_name: None }).await {
                                Ok(CommandResult::ThreadCreated { thread_id }) =>
                                    navigate(route, Route { workspace: Some(workspace), thread: Some(thread_id.0) }),
                                Ok(_) => {}
                                Err(message) => error.set(message),
                            }
                        });
                    },
                    "New Thread"
                }
                if let Some(controller) = controller {
                    for thread in controller.threads().iter().cloned() {
                        button {
                            class: "item",
                            onclick: {
                                let workspace = workspace.clone();
                                move |_| navigate(route, Route { workspace: Some(workspace.clone()), thread: Some(thread.id.0) })
                            },
                            {thread.display_name.clone().unwrap_or_else(|| format!("Thread {}", thread.id.0))}
                            br {}
                            small { "{thread.provider} / {thread.model} / {thread.reasoning_effort}" }
                        }
                    }
                }
            }
            if let Some(thread) = selected_thread {
                ThreadView { key: "{workspace}:{thread}", workspace: workspace.clone(), thread, controller_connected: remote.read().connected }
            } else {
                article { id: "transcript", p { "Select or create a Thread." } }
            }
        }
    }
}

#[component]
fn ThreadView(workspace: String, thread: i64, controller_connected: bool) -> Element {
    let mut remote = use_signal(RemoteState::<ThreadState>::default);
    let mut error = use_signal(String::new);
    let mut draft = use_signal(|| storage_get(&draft_key(&workspace, thread)));
    let workspace_for_stream = workspace.clone();
    let _thread_stream = use_hook(move || {
        connect_sse(
            &format!("/api/workspaces/{workspace_for_stream}/threads/{thread}/events"),
            move |data| match serde_json::from_str::<ThreadSubscriptionMessage>(&data) {
                Ok(message) => remote.write().apply(message),
                Err(parse_error) => error.set(parse_error.to_string()),
            },
            move |connected| {
                if !connected { remote.write().connected = false; }
            },
        )
    });
    let connected = controller_connected && remote.read().connected;
    let state = remote.read().value.clone();
    let workspace_for_send = workspace.clone();

    rsx! {
        article { id: "transcript", aria_live: "polite",
            if let Some(state) = state.clone() {
                for item in transcript(&state) {
                    section { class: if item.active { "item active" } else { "item" },
                        small { "{item.label}" }
                        pre { "{item.body}" }
                    }
                }
                if let Some(outcome) = state.last_outcome() {
                    section { class: "item", strong { "Turn outcome" } pre { "{outcome:?}" } }
                }
            }
        }
        form {
            id: "composer",
            onsubmit: move |event| {
                event.prevent_default();
                let message = draft();
                if message.trim().is_empty() { return; }
                let workspace = workspace_for_send.clone();
                spawn(async move {
                    match command(&workspace, &Command::ThreadSend {
                        thread_id: ThreadId(thread), message: message.clone(), allow_questions: true,
                    }).await {
                        Ok(_) => {
                            storage_set(&draft_key(&workspace, thread), "");
                            draft.set(String::new());
                        }
                        Err(message) => error.set(message),
                    }
                });
            },
            label { r#for: "message", "Message" }
            textarea {
                id: "message",
                value: "{draft}",
                oninput: move |event| {
                    let value = event.value();
                    storage_set(&draft_key(&workspace, thread), &value);
                    draft.set(value);
                }
            }
            button { r#type: "submit", disabled: !connected, "Send" }
        }
        aside { class: "details",
            h2 { "Thread details" }
            if !error().is_empty() { p { role: "alert", "{error}" } }
            if let Some(state) = state {
                p { "{state.metadata().provider} / {state.metadata().model} / {state.metadata().reasoning_effort}" }
                ActionButton { workspace: workspace.clone(), command: Command::ThreadCancel { thread_id: ThreadId(thread) }, label: "Cancel turn", disabled: !connected }
                ActionButton { workspace: workspace.clone(), command: Command::ThreadContinue { thread_id: ThreadId(thread), allow_questions: true }, label: "Continue", disabled: !connected }
                ActionButton { workspace: workspace.clone(), command: Command::ThreadCompact { thread_id: ThreadId(thread), allow_questions: true }, label: "Compact", disabled: !connected }
                ActionButton { workspace: workspace.clone(), command: Command::ThreadCheckpointCreate { thread_id: ThreadId(thread) }, label: "Create checkpoint", disabled: !connected }
                h3 { "Checkpoints" }
                for checkpoint in state.checkpoints().iter() {
                    div { class: "item", "Checkpoint {checkpoint.id.0}: {checkpoint.reason}" }
                }
                h3 { "Processes" }
                for process in state.processes().iter() {
                    div { class: "item",
                        code { "{process.command()}" }
                        small { " {process.status():?}" }
                        button {
                            disabled: !connected,
                            onclick: {
                                let workspace = workspace.clone();
                                let locator = process.locator().clone();
                                move |_| {
                                    let confirmed = web_sys::window()
                                        .and_then(|window| window.confirm_with_message("Stop this process?").ok())
                                        .unwrap_or(false);
                                    if confirmed {
                                        let workspace = workspace.clone();
                                        let locator = locator.clone();
                                        spawn(async move { let _ = command(&workspace, &Command::StopProcess { process: locator }).await; });
                                    }
                                }
                            },
                            "Stop"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ActionButton(workspace: String, command: Command, label: &'static str, disabled: bool) -> Element {
    rsx! {
        button {
            disabled,
            onclick: move |_| {
                let workspace = workspace.clone();
                let command = command.clone();
                spawn(async move { let _ = self::command(&workspace, &command).await; });
            },
            "{label}"
        }
    }
}
