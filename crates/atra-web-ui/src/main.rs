mod model;

use std::{cell::RefCell, rc::Rc};

use atra_protocol::{
    AgentStatus, CheckpointId, CheckpointState, CheckpointSubscriptionMessage, Command,
    CommandResult, ControllerOperation, ControllerState, ControllerSubscriptionMessage,
    HistoryTarget, InteractionId, ProcessId, ProcessState, ProcessSubscriptionMessage,
    QuestionAnswer, ThreadId, ThreadState, ThreadSubscriptionMessage,
};
use dioxus::prelude::*;
use gloo_net::http::Request;
use model::{
    Detail, NOTIFICATIONS_KEY, RemoteState, Route, THEME_KEY, WorkspaceList, draft_key,
    history_key, render_markdown, transcript,
};
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::JsFuture;
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
    let on_open = Closure::wrap(
        Box::new(move |_event: Event| (open_status.borrow_mut())(true)) as Box<dyn FnMut(Event)>,
    );
    source.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    let on_error = Closure::wrap(
        Box::new(move |_event: Event| (status.borrow_mut())(false)) as Box<dyn FnMut(Event)>
    );
    source.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    Some(Rc::new(SseConnection {
        source,
        _message: on_message,
        _open: on_open,
        _error: on_error,
    }))
}

struct PopstateListener {
    _callback: Closure<dyn FnMut(Event)>,
}

impl PopstateListener {
    fn new(mut route: Signal<Route>) -> Self {
        let callback = Closure::wrap(
            Box::new(move |_event: Event| route.set(browser_route())) as Box<dyn FnMut(Event)>
        );
        if let Some(window) = web_sys::window() {
            window.set_onpopstate(Some(callback.as_ref().unchecked_ref()));
        }
        Self {
            _callback: callback,
        }
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
    if let Some(storage) =
        web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    {
        let _ = if value.is_empty() {
            storage.remove_item(key)
        } else {
            storage.set_item(key, value)
        };
    }
}

fn confirm(message: &str) -> bool {
    web_sys::window()
        .and_then(|window| window.confirm_with_message(message).ok())
        .unwrap_or(false)
}

fn prompt(message: &str, default: &str) -> Option<String> {
    web_sys::window()
        .and_then(|window| {
            window
                .prompt_with_message_and_default(message, default)
                .ok()
        })
        .flatten()
}

fn save_sent_message(workspace: &str, message: &str) {
    let key = history_key(workspace);
    let mut history = serde_json::from_str::<Vec<String>>(&storage_get(&key)).unwrap_or_default();
    history.retain(|current| current != message);
    history.push(message.to_owned());
    if history.len() > 50 {
        history.drain(..history.len() - 50);
    }
    if let Ok(value) = serde_json::to_string(&history) {
        storage_set(&key, &value);
    }
}

fn request_notification_permission() {
    spawn(async {
        if let Ok(permission) = web_sys::Notification::request_permission() {
            let _ = JsFuture::from(permission).await;
        }
    });
}

fn notify(title: &str) {
    if web_sys::Notification::permission() == web_sys::NotificationPermission::Granted {
        let _ = web_sys::Notification::new(title);
    }
}

fn copy_text(value: String) {
    spawn(async move {
        if let Some(window) = web_sys::window() {
            let _ = JsFuture::from(window.navigator().clipboard().write_text(&value)).await;
        }
    });
}

async fn command(workspace: &str, command: &Command) -> Result<CommandResult, String> {
    let response = Request::post(&format!("/api/workspaces/{workspace}/commands"))
        .header("Content-Type", "application/json")
        .json(command)
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.ok() {
        response.json().await.map_err(|error| error.to_string())
    } else {
        let value: serde_json::Value = response.json().await.map_err(|error| error.to_string())?;
        Err(value
            .get("error")
            .and_then(|message| message.as_str())
            .unwrap_or("command failed")
            .to_owned())
    }
}

#[component]
fn App() -> Element {
    let route = use_signal(browser_route);
    let _popstate = use_hook(move || Rc::new(PopstateListener::new(route)));
    let mut workspaces = use_signal(WorkspaceList::default);
    let mut daemon_connected = use_signal(|| false);
    let mut theme = use_signal(|| {
        let value = storage_get(THEME_KEY);
        if value == "light" {
            "light".to_owned()
        } else {
            "dark".to_owned()
        }
    });
    let mut notifications = use_signal(|| storage_get(NOTIFICATIONS_KEY) == "enabled");
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
        document::Link { rel: "stylesheet", href: asset!("/assets/tailwind.css") }
        main { class: "shell", "data-theme": "{theme}",
            aside { class: "navigation",
                h1 { "Atra" }
                p { "Web Client " span { class: "status", "experimental" } }
                p { class: "connection",
                    if daemon_connected() { "Daemon connected" } else { "Daemon reconnecting…" }
                }
                nav { aria_label: "Workspaces",
                    if workspaces.read().workspaces.is_empty() {
                        p { "No running Workspaces." }
                    }
                    for workspace in workspaces.read().workspaces.clone() {
                        div { class: "item workspace-entry",
                            button {
                                class: "workspace-button",
                                onclick: {
                                    let workspace_id = workspace.workspace_id.clone();
                                    move |_| navigate(route, Route {
                                        workspace: Some(workspace_id.clone()),
                                        thread: None,
                                        detail: None,
                                    })
                                },
                                strong { "{workspace.name}" }
                                br {}
                                small { "{workspace.path}" }
                            }
                            WorkspaceMonitor {
                                workspace_id: workspace.workspace_id.clone(),
                                workspace_name: workspace.name.clone(),
                            }
                        }
                    }
                }
                details {
                    summary { "Display settings" }
                    label {
                        "Theme"
                        select {
                            value: "{theme}",
                            onchange: move |event| {
                                let value = event.value();
                                storage_set(THEME_KEY, &value);
                                theme.set(value);
                            },
                            option { value: "dark", "Dark" }
                            option { value: "light", "Light" }
                        }
                    }
                    label { class: "check-row",
                        input {
                            r#type: "checkbox",
                            checked: notifications(),
                            onchange: move |event| {
                                let enabled = event.checked();
                                storage_set(NOTIFICATIONS_KEY, if enabled { "enabled" } else { "" });
                                if enabled {
                                    request_notification_permission();
                                }
                                notifications.set(enabled);
                            }
                        }
                        "Browser notifications"
                    }
                    small { "Notifications are emitted only while this page is connected." }
                }
            }
            if let Some(workspace) = current.workspace {
                ControllerView {
                    key: "{workspace}",
                    workspace,
                    selected_thread: current.thread,
                    detail: current.detail,
                    route,
                }
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
fn WorkspaceMonitor(workspace_id: String, workspace_name: String) -> Element {
    let mut remote = use_signal(RemoteState::<ControllerState>::default);
    let id_for_stream = workspace_id.clone();
    let name_for_stream = workspace_name.clone();
    let _stream = use_hook(move || {
        let statuses = Rc::new(RefCell::new(std::collections::HashMap::new()));
        connect_sse(
            &format!("/api/workspaces/{id_for_stream}/controller/events"),
            move |data| {
                if let Ok(message) = serde_json::from_str::<ControllerSubscriptionMessage>(&data) {
                    match &message {
                        ControllerSubscriptionMessage::Snapshot { state } => {
                            let mut current = statuses.borrow_mut();
                            current.clear();
                            for thread in state.threads() {
                                if let Some(status) = state.thread_status(thread.id) {
                                    current.insert(thread.id.0, status);
                                }
                            }
                        }
                        ControllerSubscriptionMessage::Operation {
                            operation:
                                ControllerOperation::ThreadStatusUpdated { thread_id, status },
                        } => {
                            let previous = statuses.borrow_mut().insert(thread_id.0, *status);
                            let thread_name = remote
                                .read()
                                .value
                                .as_ref()
                                .and_then(|state| {
                                    state
                                        .threads()
                                        .iter()
                                        .find(|thread| thread.id == *thread_id)
                                        .and_then(|thread| thread.display_name.clone())
                                })
                                .unwrap_or_else(|| format!("Thread {}", thread_id.0));
                            if previous != Some(*status)
                                && storage_get(NOTIFICATIONS_KEY) == "enabled"
                                && matches!(
                                    status,
                                    AgentStatus::AwaitingApproval | AgentStatus::AwaitingQuestion
                                )
                            {
                                let category = match status {
                                    AgentStatus::AwaitingApproval => "approval required",
                                    AgentStatus::AwaitingQuestion => "question awaiting answer",
                                    _ => unreachable!(),
                                };
                                notify(&format!("{name_for_stream} · {thread_name} · {category}"));
                            }
                        }
                        _ => {}
                    }
                    remote.write().apply(message);
                }
            },
            move |connected| {
                if !connected {
                    remote.write().connected = false;
                }
            },
        )
    });
    let (running, awaiting) = remote
        .read()
        .value
        .as_ref()
        .map(|state| {
            state
                .threads()
                .iter()
                .fold((0, 0), |(running, awaiting), thread| {
                    match state.thread_status(thread.id) {
                        Some(AgentStatus::Running | AgentStatus::Compacting) => {
                            (running + 1, awaiting)
                        }
                        Some(AgentStatus::AwaitingApproval | AgentStatus::AwaitingQuestion) => {
                            (running, awaiting + 1)
                        }
                        _ => (running, awaiting),
                    }
                })
        })
        .unwrap_or((0, 0));
    rsx! {
        small { class: "workspace-summary",
            if remote.read().connected {
                "{running} running · {awaiting} awaiting"
            } else {
                "reconnecting…"
            }
        }
    }
}

#[component]
fn ControllerView(
    workspace: String,
    selected_thread: Option<i64>,
    detail: Option<Detail>,
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
                if !connected {
                    remote.write().connected = false;
                }
            },
        )
    });
    let controller = remote.read().value.clone();
    let connected = remote.read().connected;
    let workspace_for_create = workspace.clone();

    rsx! {
        section { class: "conversation",
            header { class: "connection-header",
                strong { if connected { "Connected" } else { "Read only — reconnecting…" } }
                if !error().is_empty() { p { role: "alert", "{error}" } }
            }
            nav { class: "thread-list", aria_label: "Threads",
                button {
                    disabled: !connected,
                    onclick: move |_| {
                        let workspace = workspace_for_create.clone();
                        spawn(async move {
                            match command(&workspace, &Command::ThreadCreate { display_name: None }).await {
                                Ok(CommandResult::ThreadCreated { thread_id }) => navigate(route, Route {
                                    workspace: Some(workspace),
                                    thread: Some(thread_id.0),
                                    detail: None,
                                }),
                                Ok(_) => error.set("Controller returned an unexpected result".to_owned()),
                                Err(message) => error.set(message),
                            }
                        });
                    },
                    "New Thread"
                }
                if let Some(controller) = controller.clone() {
                    for thread in controller.threads().iter().cloned() {
                        button {
                            class: "item thread-button",
                            onclick: {
                                let workspace = workspace.clone();
                                move |_| navigate(route, Route {
                                    workspace: Some(workspace.clone()),
                                    thread: Some(thread.id.0),
                                    detail: None,
                                })
                            },
                            span {
                                {thread.display_name.clone().unwrap_or_else(|| format!("Thread {}", thread.id.0))}
                                if let Some(status) = controller.thread_status(thread.id) {
                                    span { class: "status", "{status:?}" }
                                }
                            }
                            br {}
                            small { "{thread.provider} / {thread.model} / {thread.reasoning_effort}" }
                        }
                    }
                }
            }
            if let Some(thread) = selected_thread {
                ThreadView {
                    key: "{workspace}:{thread}",
                    workspace: workspace.clone(),
                    thread,
                    detail,
                    controller,
                    controller_connected: connected,
                    route,
                }
            } else {
                article { id: "transcript", p { "Select or create a Thread." } }
            }
        }
    }
}

#[component]
fn ThreadView(
    workspace: String,
    thread: i64,
    detail: Option<Detail>,
    controller: Option<ControllerState>,
    controller_connected: bool,
    route: Signal<Route>,
) -> Element {
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
                if !connected {
                    remote.write().connected = false;
                }
            },
        )
    });
    let connected = controller_connected && remote.read().connected;
    let state = remote.read().value.clone();
    let workspace_for_send = workspace.clone();
    let workspace_for_draft = workspace.clone();
    let workspace_for_shortcut = workspace.clone();
    use_effect(move || {
        let _ = remote
            .read()
            .value
            .as_ref()
            .map(|state| state.events().len());
        if let Some(element) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id("transcript"))
        {
            element.set_scroll_top(element.scroll_height());
        }
    });

    rsx! {
        article { id: "transcript", aria_live: "polite",
            if let Some(state) = state.clone() {
                for item in transcript(&state) {
                    section { class: if item.active { "item transcript-item active" } else { "item transcript-item" },
                        small { "{item.label}" }
                        if item.markdown {
                            div {
                                class: "markdown",
                                dangerous_inner_html: "{render_markdown(&item.body)}",
                            }
                        } else if item.body.len() > 4000 {
                            details {
                                summary { "Show long output ({item.body.len()} characters)" }
                                pre { "{item.body}" }
                            }
                        } else {
                            pre { "{item.body}" }
                        }
                        button {
                            class: "copy-button",
                            onclick: {
                                let body = item.body.clone();
                                move |_| copy_text(body.clone())
                            },
                            "Copy"
                        }
                    }
                }
                if let Some(outcome) = state.last_outcome() {
                    section { class: "item", strong { "Turn outcome" } pre { "{outcome:?}" } }
                }
                if let Some(turn) = state.active_turn() {
                    if let Some(approval) = turn.pending_approval() {
                        ApprovalForm {
                            workspace: workspace.clone(),
                            id: approval.id(),
                            tool: approval.tool().to_owned(),
                            arguments: serde_json::to_string_pretty(approval.arguments()).unwrap_or_default(),
                            connected,
                            error,
                        }
                    }
                    if let Some(request) = turn.pending_question() {
                        QuestionForm {
                            workspace: workspace.clone(),
                            request: request.clone(),
                            connected,
                            error,
                        }
                    }
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
                        thread_id: ThreadId(thread),
                        message: message.clone(),
                        allow_questions: true,
                    }).await {
                        Ok(_) => {
                            save_sent_message(&workspace, &message);
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
                    storage_set(&draft_key(&workspace_for_draft, thread), &value);
                    draft.set(value);
                },
                onkeydown: move |event| {
                    if (event.modifiers().contains(Modifiers::CONTROL)
                        || event.modifiers().contains(Modifiers::META))
                        && event.key() == Key::Enter
                    {
                        event.prevent_default();
                        if let Some(form) = web_sys::window()
                            .and_then(|window| window.document())
                            .and_then(|document| document.get_element_by_id("composer"))
                            .and_then(|element| element.dyn_into::<web_sys::HtmlFormElement>().ok())
                        {
                            let _ = form.request_submit();
                        }
                    } else if event.key() == Key::Escape && connected {
                        let workspace = workspace_for_shortcut.clone();
                        spawn(async move {
                            if let Err(message) = command(&workspace, &Command::ThreadCancel {
                                thread_id: ThreadId(thread),
                            }).await {
                                error.set(message);
                            }
                        });
                    }
                }
            }
            div { class: "button-row",
                button { r#type: "submit", disabled: !connected, "Send" }
                if let Ok(history) = serde_json::from_str::<Vec<String>>(&storage_get(&history_key(&workspace))) {
                    if let Some(last) = history.last() {
                        button {
                            r#type: "button",
                            onclick: {
                                let last = last.clone();
                                move |_| draft.set(last.clone())
                            },
                            "Recall last sent"
                        }
                    }
                }
            }
        }
        aside { class: "details",
            h2 { "Thread details" }
            if !error().is_empty() { p { role: "alert", "{error}" } }
            if let Some(state) = state {
                ThreadControls {
                    workspace: workspace.clone(),
                    thread,
                    state: state.clone(),
                    controller,
                    connected,
                    route,
                    error,
                }
                if let Some(detail) = detail {
                    match detail {
                        Detail::Checkpoint(checkpoint) => rsx! {
                            CheckpointView {
                                workspace: workspace.clone(),
                                thread,
                                checkpoint,
                                connected,
                                route,
                                error,
                            }
                        },
                        Detail::Process { runner, process } => rsx! {
                            ProcessView {
                                workspace: workspace.clone(),
                                thread,
                                runner,
                                process,
                                connected,
                                error,
                            }
                        },
                    }
                }
            }
        }
    }
}

#[component]
fn ThreadControls(
    workspace: String,
    thread: i64,
    state: ThreadState,
    controller: Option<ControllerState>,
    connected: bool,
    route: Signal<Route>,
    error: Signal<String>,
) -> Element {
    let metadata = state.metadata().clone();
    let workspace_for_rename = workspace.clone();
    let workspace_for_delete = workspace.clone();
    let workspace_for_model = workspace.clone();
    let mut selected_model = use_signal(|| format!("{}\n{}", metadata.provider, metadata.model));
    let mut reasoning = use_signal(|| metadata.reasoning_effort.clone());
    let models = controller
        .as_ref()
        .map(|controller| {
            controller
                .providers()
                .iter()
                .flat_map(|provider| provider.models().iter().cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let selected_model_value = selected_model();
    let efforts = models
        .iter()
        .find(|model| format!("{}\n{}", model.provider, model.id) == selected_model_value)
        .map(|model| model.supported_reasoning_efforts.clone())
        .unwrap_or_else(|| vec![reasoning()]);
    let models_for_change = models.clone();

    rsx! {
        p { "{metadata.provider} / {metadata.model} / {metadata.reasoning_effort}" }
        div { class: "button-row",
            button {
                disabled: !connected,
                onclick: move |_| {
                    if let Some(name) = prompt(
                        "Thread name",
                        metadata.display_name.as_deref().unwrap_or(""),
                    ) {
                        let workspace = workspace_for_rename.clone();
                        spawn(async move {
                            if let Err(message) = command(&workspace, &Command::ThreadRename {
                                thread_id: ThreadId(thread),
                                display_name: name,
                            }).await {
                                error.set(message);
                            }
                        });
                    }
                },
                "Rename"
            }
            button {
                class: "danger",
                disabled: !connected,
                onclick: move |_| {
                    if !confirm("Delete this Thread and its descendants?") { return; }
                    let workspace = workspace_for_delete.clone();
                    spawn(async move {
                        match command(&workspace, &Command::ThreadDeleteRecursive {
                            thread_id: ThreadId(thread),
                        }).await {
                            Ok(_) => navigate(route, Route {
                                workspace: Some(workspace),
                                thread: None,
                                detail: None,
                            }),
                            Err(message) => error.set(message),
                        }
                    });
                },
                "Delete"
            }
        }
        if !models.is_empty() {
            form {
                class: "settings-form",
                onsubmit: move |event| {
                    event.prevent_default();
                    let Some((provider, model)) = selected_model().split_once('\n').map(|(a, b)| (a.to_owned(), b.to_owned())) else {
                        return;
                    };
                    let workspace = workspace_for_model.clone();
                    let effort = reasoning();
                    spawn(async move {
                        if let Err(message) = command(&workspace, &Command::ThreadSetModel {
                            thread_id: ThreadId(thread),
                            provider,
                            model,
                            reasoning_effort: effort,
                        }).await {
                            error.set(message);
                        }
                    });
                },
                label {
                    "Model"
                    select {
                        value: "{selected_model}",
                        onchange: move |event| {
                            let value = event.value();
                            if let Some(model) = models_for_change.iter().find(|model| {
                                format!("{}\n{}", model.provider, model.id) == value
                            }) {
                                reasoning.set(model.default_reasoning_effort.clone());
                            }
                            selected_model.set(value);
                        },
                        for model in models.iter() {
                            option {
                                value: "{model.provider}\n{model.id}",
                                "{model.display_name} ({model.provider})"
                            }
                        }
                    }
                }
                label {
                    "Reasoning effort"
                    select {
                        value: "{reasoning}",
                        onchange: move |event| reasoning.set(event.value()),
                        for effort in efforts {
                            option { value: "{effort}", "{effort}" }
                        }
                    }
                }
                button { disabled: !connected, r#type: "submit", "Apply model" }
            }
        }
        div { class: "button-row",
            ActionButton {
                workspace: workspace.clone(),
                command_value: Command::ThreadCancel { thread_id: ThreadId(thread) },
                label: "Cancel turn",
                disabled: !connected,
                error,
            }
            ActionButton {
                workspace: workspace.clone(),
                command_value: Command::ThreadContinue { thread_id: ThreadId(thread), allow_questions: true },
                label: "Continue",
                disabled: !connected,
                error,
            }
            ActionButton {
                workspace: workspace.clone(),
                command_value: Command::ThreadCompact { thread_id: ThreadId(thread), allow_questions: true },
                label: "Compact",
                disabled: !connected,
                error,
            }
            ActionButton {
                workspace: workspace.clone(),
                command_value: Command::ThreadCheckpointCreate { thread_id: ThreadId(thread) },
                label: "Create checkpoint",
                disabled: !connected,
                error,
            }
        }
        h3 { "Checkpoints" }
        for checkpoint in state.checkpoints().iter() {
            div { class: "item",
                button {
                    class: "link-button",
                    onclick: {
                        let workspace = workspace.clone();
                        let id = checkpoint.id.0;
                        move |_| navigate(route, Route {
                            workspace: Some(workspace.clone()),
                            thread: Some(thread),
                            detail: Some(Detail::Checkpoint(id)),
                        })
                    },
                    "Checkpoint {checkpoint.id.0}"
                }
                p { "{checkpoint.reason}" }
            }
        }
        h3 { "Processes" }
        for process in state.processes().iter() {
            div { class: "item",
                button {
                    class: "link-button",
                    onclick: {
                        let workspace = workspace.clone();
                        let runner = process.locator().runner().to_owned();
                        let process_id = process.locator().process_id().0.clone();
                        move |_| navigate(route, Route {
                            workspace: Some(workspace.clone()),
                            thread: Some(thread),
                            detail: Some(Detail::Process {
                                runner: runner.clone(),
                                process: process_id.clone(),
                            }),
                        })
                    },
                    code { "{process.command()}" }
                }
                small { " {process.status():?}" }
            }
        }
    }
}

#[component]
fn ApprovalForm(
    workspace: String,
    id: InteractionId,
    tool: String,
    arguments: String,
    connected: bool,
    error: Signal<String>,
) -> Element {
    let mut reason = use_signal(String::new);
    rsx! {
        section { class: "interaction", role: "alert",
            h3 { "Approval required" }
            strong { "{tool}" }
            pre { "{arguments}" }
            label {
                "Denial reason (optional)"
                input { value: "{reason}", oninput: move |event| reason.set(event.value()) }
            }
            div { class: "button-row",
                ActionButton {
                    workspace: workspace.clone(),
                    command_value: Command::ApprovalAllow { approval_id: id },
                    label: "Allow",
                    disabled: !connected,
                    error,
                }
                button {
                    class: "danger",
                    disabled: !connected,
                    onclick: move |_| {
                        let workspace = workspace.clone();
                        let reason = reason();
                        spawn(async move {
                            if let Err(message) = command(&workspace, &Command::ApprovalDeny {
                                approval_id: id,
                                reason: if reason.trim().is_empty() { None } else { Some(reason) },
                            }).await {
                                error.set(message);
                            }
                        });
                    },
                    "Deny"
                }
            }
        }
    }
}

#[component]
fn QuestionForm(
    workspace: String,
    request: atra_protocol::PendingQuestionRequest,
    connected: bool,
    error: Signal<String>,
) -> Element {
    let request_id = request.id;
    let mut answers = use_signal(|| {
        request
            .questions
            .iter()
            .map(|question| QuestionAnswer {
                selected_option: question
                    .recommended_options
                    .first()
                    .cloned()
                    .or_else(|| question.options.first().map(|option| option.label.clone())),
                note: String::new(),
            })
            .collect::<Vec<_>>()
    });

    rsx! {
        form {
            class: "interaction",
            onsubmit: move |event| {
                event.prevent_default();
                let workspace = workspace.clone();
                let values = answers();
                spawn(async move {
                    if let Err(message) = command(&workspace, &Command::QuestionAnswer {
                        request_id,
                        answers: values,
                    }).await {
                        error.set(message);
                    }
                });
            },
            h3 { "Questions" }
            for (index, question) in request.questions.iter().enumerate() {
                fieldset {
                    legend { "{question.question}" }
                    select {
                        value: "{answers.read()[index].selected_option.clone().unwrap_or_default()}",
                        onchange: move |event| {
                            answers.write()[index].selected_option = if event.value().is_empty() {
                                None
                            } else {
                                Some(event.value())
                            };
                        },
                        option { value: "", "None of these" }
                        for option in question.options.iter() {
                            option { value: "{option.label}", "{option.label} — {option.description}" }
                        }
                    }
                    input {
                        placeholder: "Optional note",
                        value: "{answers.read()[index].note}",
                        oninput: move |event| answers.write()[index].note = event.value(),
                    }
                }
            }
            button { r#type: "submit", disabled: !connected, "Submit answers" }
        }
    }
}

#[component]
fn CheckpointView(
    workspace: String,
    thread: i64,
    checkpoint: i64,
    connected: bool,
    route: Signal<Route>,
    error: Signal<String>,
) -> Element {
    let mut remote = use_signal(RemoteState::<CheckpointState>::default);
    let workspace_for_stream = workspace.clone();
    let _stream = use_hook(move || {
        connect_sse(
            &format!(
                "/api/workspaces/{workspace_for_stream}/threads/{thread}/checkpoints/{checkpoint}/events"
            ),
            move |data| match serde_json::from_str::<CheckpointSubscriptionMessage>(&data) {
                Ok(message) => remote.write().apply(message),
                Err(parse_error) => error.set(parse_error.to_string()),
            },
            move |is_connected| {
                if !is_connected {
                    remote.write().connected = false;
                }
            },
        )
    });
    let state = remote.read().value.clone();

    rsx! {
        section { class: "detail-view",
            h3 { "Checkpoint {checkpoint}" }
            if let Some(state) = state {
                p { "{state.metadata().reason}" }
                p { "{state.events().len()} events" }
                if let Some(sequence) = state.events().last().map(|event| event.sequence) {
                    div { class: "button-row",
                        button {
                            disabled: !connected,
                            onclick: {
                                let workspace = workspace.clone();
                                move |_| {
                                    let workspace = workspace.clone();
                                    spawn(async move {
                                        match command(&workspace, &Command::ThreadFork {
                                            thread_id: ThreadId(thread),
                                            checkpoint_id: Some(CheckpointId(checkpoint)),
                                            sequence,
                                            display_name: None,
                                        }).await {
                                            Ok(CommandResult::ThreadForked { thread_id }) => navigate(route, Route {
                                                workspace: Some(workspace),
                                                thread: Some(thread_id.0),
                                                detail: None,
                                            }),
                                            Ok(_) => error.set("Controller returned an unexpected result".to_owned()),
                                            Err(message) => error.set(message),
                                        }
                                    });
                                }
                            },
                            "Fork"
                        }
                        button {
                            class: "danger",
                            disabled: !connected,
                            onclick: {
                                let workspace = workspace.clone();
                                move |_| {
                                    if !confirm("Rewind this Thread to the checkpoint's last message?") { return; }
                                    let workspace = workspace.clone();
                                    spawn(async move {
                                        if let Err(message) = command(&workspace, &Command::ThreadReplaceHistory {
                                            thread_id: ThreadId(thread),
                                            target: HistoryTarget::Message {
                                                checkpoint_id: Some(CheckpointId(checkpoint)),
                                                sequence,
                                            },
                                        }).await {
                                            error.set(message);
                                        }
                                    });
                                }
                            },
                            "Rewind"
                        }
                    }
                }
                button {
                    class: "danger",
                    disabled: !connected,
                    onclick: move |_| {
                        if !confirm("Restore this checkpoint and replace current Thread history?") { return; }
                        let workspace = workspace.clone();
                        spawn(async move {
                            if let Err(message) = command(&workspace, &Command::ThreadReplaceHistory {
                                thread_id: ThreadId(thread),
                                target: HistoryTarget::Checkpoint { checkpoint_id: CheckpointId(checkpoint) },
                            }).await {
                                error.set(message);
                            }
                        });
                    },
                    "Restore checkpoint"
                }
                details {
                    summary { "Checkpoint transcript" }
                    for item in state.events().iter() {
                        pre { class: "item", "{serde_json::to_string_pretty(item).unwrap_or_default()}" }
                    }
                }
            } else {
                p { "Loading checkpoint…" }
            }
        }
    }
}

#[component]
fn ProcessView(
    workspace: String,
    thread: i64,
    runner: String,
    process: String,
    connected: bool,
    error: Signal<String>,
) -> Element {
    let mut remote = use_signal(RemoteState::<ProcessState>::default);
    let workspace_for_stream = workspace.clone();
    let runner_for_stream = runner.clone();
    let process_for_stream = process.clone();
    let _stream = use_hook(move || {
        connect_sse(
            &format!(
                "/api/workspaces/{workspace_for_stream}/runners/{runner_for_stream}/processes/{process_for_stream}/events?thread_id={thread}"
            ),
            move |data| match serde_json::from_str::<ProcessSubscriptionMessage>(&data) {
                Ok(message) => remote.write().apply(message),
                Err(parse_error) => error.set(parse_error.to_string()),
            },
            move |is_connected| {
                if !is_connected {
                    remote.write().connected = false;
                }
            },
        )
    });
    let state = remote.read().value.clone();

    rsx! {
        section { class: "detail-view",
            h3 { "Process {process}" }
            if let Some(state) = state {
                p { code { "{state.process().command()}" } }
                p { "{state.process().status():?}" }
                if state.omitted_bytes() > 0 {
                    small { "{state.omitted_bytes()} earlier bytes omitted" }
                }
                pre { class: "process-output", "{state.output_tail()}" }
                button {
                    class: "danger",
                    disabled: !connected,
                    onclick: move |_| {
                        if !confirm("Stop this managed process?") { return; }
                        let workspace = workspace.clone();
                        let locator = atra_protocol::ProcessLocator::new(
                            ThreadId(thread),
                            runner.clone(),
                            ProcessId(process.clone()),
                        );
                        spawn(async move {
                            if let Err(message) = command(&workspace, &Command::StopProcess {
                                process: locator,
                            }).await {
                                error.set(message);
                            }
                        });
                    },
                    "Stop process"
                }
            } else {
                p { "Loading process…" }
            }
        }
    }
}

#[component]
fn ActionButton(
    workspace: String,
    command_value: Command,
    label: &'static str,
    disabled: bool,
    error: Signal<String>,
) -> Element {
    rsx! {
        button {
            disabled,
            onclick: move |_| {
                let workspace = workspace.clone();
                let command_value = command_value.clone();
                spawn(async move {
                    if let Err(message) = command(&workspace, &command_value).await {
                        error.set(message);
                    }
                });
            },
            "{label}"
        }
    }
}
