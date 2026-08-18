mod model;
mod syntax;
mod thread_store;
mod transcript_view;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use atra_protocol::{
    AgentStatus, CheckpointId, CheckpointSubscriptionMessage, Command, CommandResult,
    ControllerOperation, ControllerState, ControllerSubscriptionMessage, EventSequence,
    HistoryTarget, InteractionId, PendingInteraction, ProcessId, ProcessState, ProcessStatus,
    ProcessSubscriptionMessage, QuestionAnswer, ThreadId, ThreadState, ThreadSubscriptionMessage,
};
use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use gloo_net::http::Request;
use gloo_timers::future::TimeoutFuture;
use model::{
    Detail, LAST_ROUTE_KEY, NAV_OPEN_KEY, NAV_WIDTH_KEY, NOTIFICATIONS_KEY, PINS_KEY, Pin,
    RemoteState, Route, THEME_KEY, TranscriptMode, UTILITY_OPEN_KEY, UTILITY_TAB_KEY,
    UTILITY_WIDTH_KEY, UtilityTab, WORKSPACE_COLLAPSE_KEY, Workspace, WorkspaceList, draft_key,
    factual_status, family_threads, history_key, render_markdown, root_id, root_threads,
    thread_name,
};
use syntax::{highlight, setup_markdown_highlighting};
use thread_store::ThreadStore;
use transcript_view::{ActivityDisplay, ActivityKey, CommandDisplay, RawKey, TurnKey};
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    ErrorEvent, Event as WebEvent, EventSource, HtmlDialogElement, HtmlFormElement,
    HtmlTextAreaElement, MessageEvent, PromiseRejectionEvent,
};

type Controllers = HashMap<String, RemoteState<ControllerState>>;
type ThreadAttention = HashMap<(String, i64), AgentStatus>;

fn main() {
    install_browser_diagnostics();
    setup_markdown_highlighting();
    dioxus::launch(App);
}

fn install_browser_diagnostics() {
    std::panic::set_hook(Box::new(|info| {
        if let Some(window) = web_sys::window()
            && let Ok(location) = window.location().href()
        {
            web_sys::console::error_1(&format!("Atra context: {location}").into());
        }
        console_error_panic_hook::hook(info);
    }));
    let Some(window) = web_sys::window() else {
        return;
    };
    let error = Closure::<dyn FnMut(ErrorEvent)>::new(|event: ErrorEvent| {
        web_sys::console::error_1(
            &format!(
                "Atra window error: {} ({}:{}:{})",
                event.message(),
                event.filename(),
                event.lineno(),
                event.colno()
            )
            .into(),
        );
    });
    let _ = window.add_event_listener_with_callback("error", error.as_ref().unchecked_ref());
    error.forget();
    let rejection =
        Closure::<dyn FnMut(PromiseRejectionEvent)>::new(|event: PromiseRejectionEvent| {
            web_sys::console::error_2(&"Atra unhandled rejection".into(), &event.reason());
        });
    let _ = window
        .add_event_listener_with_callback("unhandledrejection", rejection.as_ref().unchecked_ref());
    rejection.forget();
}

struct SseConnection {
    source: EventSource,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _open: Closure<dyn FnMut(WebEvent)>,
    _error: Closure<dyn FnMut(WebEvent)>,
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
        Box::new(move |_event: WebEvent| (open_status.borrow_mut())(true))
            as Box<dyn FnMut(WebEvent)>,
    );
    source.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    let on_error = Closure::wrap(
        Box::new(move |_event: WebEvent| (status.borrow_mut())(false)) as Box<dyn FnMut(WebEvent)>,
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
    _callback: Closure<dyn FnMut(WebEvent)>,
}

impl PopstateListener {
    fn new(mut route: Signal<Route>) -> Self {
        let callback = Closure::wrap(Box::new(move |_event: WebEvent| route.set(browser_route()))
            as Box<dyn FnMut(WebEvent)>);
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
    storage_set(LAST_ROUTE_KEY, &next.path());
    if let Some(window) = web_sys::window() {
        let _ = window.history().and_then(|history| {
            history.push_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&next.path()))
        });
    }
    route.set(next);
}

fn replace_route(mut route: Signal<Route>, next: Route) {
    if let Some(window) = web_sys::window() {
        let _ = window.history().and_then(|history| {
            history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&next.path()))
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

fn storage_json<T: serde::de::DeserializeOwned + Default>(key: &str) -> T {
    serde_json::from_str(&storage_get(key)).unwrap_or_default()
}

fn save_json(key: &str, value: &impl serde::Serialize) {
    if let Ok(value) = serde_json::to_string(value) {
        storage_set(key, &value);
    }
}

fn save_sent_message(workspace: &str, thread: i64, message: &str) {
    let key = history_key(workspace, thread);
    let mut history = storage_json::<Vec<String>>(&key);
    history.push(message.to_owned());
    if history.len() > 100 {
        history.drain(..history.len() - 100);
    }
    save_json(&key, &history);
}

fn byte_index_at_utf16_offset(value: &str, offset: usize) -> usize {
    let mut utf16_offset = 0;
    for (byte_index, character) in value.char_indices() {
        if utf16_offset >= offset {
            return byte_index;
        }
        utf16_offset += character.len_utf16();
        if utf16_offset > offset {
            return byte_index;
        }
    }
    value.len()
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

async fn copy_text(value: String) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    JsFuture::from(window.navigator().clipboard().write_text(&value))
        .await
        .is_ok()
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
            .or_else(|| value.get("message"))
            .and_then(|message| message.as_str())
            .unwrap_or("command failed")
            .to_owned())
    }
}

fn ensure_pin(
    mut pins: Signal<Vec<Pin>>,
    workspace: &str,
    thread: ThreadId,
    controller: &ControllerState,
) {
    let root = root_id(controller.threads(), thread).0;
    if !pins
        .read()
        .iter()
        .any(|pin| pin.workspace == workspace && pin.thread == root)
    {
        pins.write().insert(
            0,
            Pin {
                workspace: workspace.to_owned(),
                thread: root,
            },
        );
        save_json(PINS_KEY, &*pins.read());
    }
}

fn set_document_title(workspace: &str, thread: &str) {
    if let Some(document) = web_sys::window().and_then(|window| window.document()) {
        document.set_title(&format!("{thread} · {workspace} · Atra"));
    }
}

fn is_narrow_viewport() -> bool {
    web_sys::window()
        .and_then(|window| window.inner_width().ok())
        .and_then(|width| width.as_f64())
        .is_some_and(|width| width <= 1180.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MobilePanel {
    None,
    Navigation,
    Utility,
}

#[derive(Clone, Copy)]
struct SwipeStart {
    x: f64,
    y: f64,
}

fn swipe_start_allowed(event: &Event<TouchData>) -> bool {
    let Some(target) = event
        .data()
        .try_as_web_event()
        .and_then(|event| event.target())
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
    else {
        return false;
    };
    if target
        .closest(".drawer-backdrop, .navigation-row > .navigation-link")
        .ok()
        .flatten()
        .is_some()
    {
        return true;
    }
    let blocks_swipe = target
        .closest(
            "button, a, input, textarea, select, summary, pre, code, table, \
             [contenteditable='true'], .composer-region, .process-output",
        )
        .ok()
        .flatten()
        .is_some();
    let has_selection = web_sys::window()
        .and_then(|window| window.get_selection().ok().flatten())
        .is_some_and(|selection| !selection.is_collapsed());
    !blocks_swipe && !has_selection
}

fn open_mobile_navigation(mut mobile_panel: Signal<MobilePanel>) {
    mobile_panel.set(MobilePanel::Navigation);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderThreadAction {
    Continue,
    Compact,
    Checkpoint,
}

#[derive(Clone, Debug, PartialEq)]
enum DialogState {
    Rename {
        current: String,
    },
    Fork {
        checkpoint: Option<i64>,
        sequence: EventSequence,
    },
    Rewind {
        checkpoint: Option<i64>,
        sequence: EventSequence,
    },
    Restore {
        checkpoint: i64,
    },
    Delete {
        name: String,
    },
    StopProcess {
        runner: String,
        process: String,
    },
}

#[component]
fn App() -> Element {
    let route = use_signal(browser_route);
    let _popstate = use_hook(move || Rc::new(PopstateListener::new(route)));
    let mut workspaces = use_signal(WorkspaceList::default);
    let controllers = use_signal(Controllers::new);
    let mut daemon_connected = use_signal(|| false);
    let theme = use_signal(|| match storage_get(THEME_KEY).as_str() {
        "light" => "light".to_owned(),
        "dark" => "dark".to_owned(),
        _ => "system".to_owned(),
    });
    let notifications = use_signal(|| storage_get(NOTIFICATIONS_KEY) == "enabled");
    let pins = use_signal(|| storage_json::<Vec<Pin>>(PINS_KEY));
    let mut attention = use_signal(ThreadAttention::new);
    let nav_open = use_signal(|| storage_get(NAV_OPEN_KEY) != "closed");
    let mut utility_open = use_signal(|| storage_get(UTILITY_OPEN_KEY) != "closed");
    let utility_tab = use_signal(|| match storage_get(UTILITY_TAB_KEY).as_str() {
        "children" => UtilityTab::Children,
        "checkpoints" => UtilityTab::Checkpoints,
        "processes" => UtilityTab::Processes,
        _ => UtilityTab::Thread,
    });
    let nav_width = use_signal(|| storage_get(NAV_WIDTH_KEY).parse::<i32>().unwrap_or(288));
    let utility_width = use_signal(|| storage_get(UTILITY_WIDTH_KEY).parse::<i32>().unwrap_or(352));
    let mut mobile_panel = use_signal(|| MobilePanel::None);
    let modes = use_signal(HashMap::<String, TranscriptMode>::new);
    let scroll_positions = use_signal(HashMap::<String, i32>::new);
    let mut route_notice = use_signal(String::new);
    let mut swipe_start = use_signal(|| None::<SwipeStart>);
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
    use_effect(move || {
        if let Some(root) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.document_element())
        {
            let _ = root.set_attribute("data-theme", &theme());
        }
    });
    use_effect(move || {
        let selected = route.read();
        if let (Some(workspace), Some(thread)) = (&selected.workspace, selected.thread) {
            attention.write().remove(&(workspace.clone(), thread));
        }
    });

    let current = route.read().clone();
    use_effect(move || {
        if route.read().workspace.is_some() || controllers.read().is_empty() {
            return;
        }
        let saved = Route::parse(&storage_get(LAST_ROUTE_KEY));
        let saved_exists = saved.workspace.as_ref().is_some_and(|workspace| {
            controllers
                .read()
                .get(workspace)
                .and_then(|remote| remote.value.as_ref())
                .is_some_and(|controller| {
                    saved.thread.is_some_and(|thread| {
                        controller.threads().iter().any(|item| item.id.0 == thread)
                    })
                })
        });
        if saved_exists {
            replace_route(route, saved);
            return;
        }
        if let Some(pin) = pins.read().first().cloned() {
            let exists = controllers
                .read()
                .get(&pin.workspace)
                .and_then(|remote| remote.value.as_ref())
                .is_some_and(|controller| {
                    controller
                        .threads()
                        .iter()
                        .any(|thread| thread.id.0 == pin.thread)
                });
            if exists {
                replace_route(
                    route,
                    Route {
                        workspace: Some(pin.workspace),
                        thread: Some(pin.thread),
                        detail: None,
                    },
                );
                return;
            }
        }
        if let Some(workspace) = saved.workspace
            && let Some(thread) = controllers
                .read()
                .get(&workspace)
                .and_then(|remote| remote.value.as_ref())
                .and_then(|controller| root_threads(controller).first().cloned())
        {
            replace_route(
                route,
                Route {
                    workspace: Some(workspace),
                    thread: Some(thread.id.0),
                    detail: None,
                },
            );
        }
    });
    use_effect(move || {
        let selected = route.read().clone();
        let Some(workspace) = selected.workspace else {
            return;
        };
        if !workspaces.read().workspaces.is_empty()
            && !workspaces
                .read()
                .workspaces
                .iter()
                .any(|item| item.workspace_id == workspace)
        {
            route_notice.set("The requested Workspace is not available.".to_owned());
            replace_route(route, Route::parse("/"));
            return;
        }
        let Some(controller) = controllers
            .read()
            .get(&workspace)
            .and_then(|remote| remote.value.clone())
        else {
            return;
        };
        if selected
            .thread
            .is_some_and(|thread| !controller.threads().iter().any(|item| item.id.0 == thread))
        {
            route_notice.set("The requested Thread is not available.".to_owned());
            replace_route(
                route,
                Route {
                    workspace: Some(workspace),
                    thread: root_threads(&controller).first().map(|thread| thread.id.0),
                    detail: None,
                },
            );
        }
    });

    let shell_class = format!(
        "app-shell{}{}",
        if nav_open() { "" } else { " navigation-closed" },
        if utility_open() {
            ""
        } else {
            " utility-closed"
        },
    );
    let shell_style = format!(
        "--navigation-width: {}px; --utility-width: {}px",
        nav_width().clamp(224, 420),
        utility_width().clamp(272, 520),
    );
    let has_thread_header = current.workspace.as_ref().is_some_and(|workspace| {
        current.thread.is_some()
            && controllers
                .read()
                .get(workspace)
                .and_then(|remote| remote.value.as_ref())
                .is_some()
    });

    rsx! {
        document::Link { rel: "stylesheet", href: asset!("/assets/tailwind.css") }
        document::Script { src: asset!("/assets/prism.min.js") }
        main {
            class: "{shell_class}",
            style: "{shell_style}",
            "data-theme": "{theme}",
            ontouchstart: move |event: Event<TouchData>| {
                if is_narrow_viewport()
                    && swipe_start_allowed(&event)
                    && event.touches().len() == 1
                {
                    let coordinates = event.touches()[0].client_coordinates();
                    swipe_start.set(Some(SwipeStart { x: coordinates.x, y: coordinates.y }));
                }
            },
            ontouchmove: move |event: Event<TouchData>| {
                let Some(start) = swipe_start() else {
                    return;
                };
                let touches = event.touches();
                if touches.len() != 1 || !is_narrow_viewport() {
                    swipe_start.set(None);
                    return;
                }
                let coordinates = touches[0].client_coordinates();
                let dx = coordinates.x - start.x;
                let dy = coordinates.y - start.y;
                if dx.abs() <= dy.abs() * 1.25 {
                    return;
                }
                event.prevent_default();
                if dx.abs() < 72.0 {
                    return;
                }
                swipe_start.set(None);
                match mobile_panel() {
                    MobilePanel::None if dx > 0.0 => {
                        open_mobile_navigation(mobile_panel);
                    }
                    MobilePanel::None if route.read().thread.is_some() => {
                        utility_open.set(true);
                        storage_set(UTILITY_OPEN_KEY, "open");
                        mobile_panel.set(MobilePanel::Utility);
                    }
                    MobilePanel::Navigation if dx < 0.0 => {
                        mobile_panel.set(MobilePanel::None);
                    }
                    MobilePanel::Utility if dx > 0.0 => {
                        mobile_panel.set(MobilePanel::None);
                    }
                    _ => {}
                }
            },
            ontouchend: move |_| swipe_start.set(None),
            ontouchcancel: move |_| swipe_start.set(None),
            for workspace in workspaces.read().workspaces.clone() {
                ControllerMonitor {
                    key: "{workspace.workspace_id}",
                    workspace: workspace.clone(),
                    controllers,
                    route,
                    attention,
                    notifications,
                }
            }
            Navigation {
                workspaces: workspaces.read().workspaces.clone(),
                controllers,
                route,
                pins,
                attention,
                nav_open,
                nav_width,
                mobile_panel,
                theme,
                notifications,
            }
            if nav_open() {
                ResizeHandle {
                    side: "Navigation",
                    value: nav_width,
                    default_value: 288,
                    min: 224,
                    max: 420,
                    storage_key: NAV_WIDTH_KEY,
                }
            }
            section {
                class: "main-thread",
                if !has_thread_header {
                    MobileShellHeader { mobile_panel }
                }
                if let Some(workspace_id) = current.workspace.clone() {
                    if let Some(remote) = controllers.read().get(&workspace_id).cloned() {
                        if let Some(controller) = remote.value {
                            if let Some(thread) = current.thread {
                                ThreadPage {
                                    key: "{workspace_id}:{thread}",
                                    workspace: workspaces
                                        .read()
                                        .workspaces
                                        .iter()
                                        .find(|item| item.workspace_id == workspace_id)
                                        .cloned()
                                        .unwrap_or(Workspace {
                                            workspace_id: workspace_id.clone(),
                                            name: workspace_id.clone(),
                                            path: String::new(),
                                        }),
                                    thread,
                                    detail: current.detail.clone(),
                                    controller,
                                    controller_connected: remote.connected,
                                    route,
                                    pins,
                                    nav_open,
                                    utility_open,
                                    utility_tab,
                                    utility_width,
                                    mobile_panel,
                                    modes,
                                    scroll_positions,
                                }
                            } else {
                                WorkspaceLanding {
                                    workspace: workspace_id,
                                    controller,
                                    connected: remote.connected,
                                    route,
                                    pins,
                                }
                            }
                        } else {
                            LoadingState { label: "Loading Workspace…" }
                        }
                    } else {
                        LoadingState { label: "Loading Workspace…" }
                    }
                } else {
                    Landing { connected: daemon_connected() }
                }
            }
            if mobile_panel() != MobilePanel::None {
                button {
                    class: "drawer-backdrop",
                    aria_label: "Close drawer",
                    onclick: move |_| mobile_panel.set(MobilePanel::None),
                }
            }
            if !route_notice().is_empty() {
                div { class: "toast route-toast", role: "status",
                    span { "{route_notice}" }
                    button {
                        aria_label: "Dismiss route notice",
                        onclick: move |_| route_notice.set(String::new()),
                        "×"
                    }
                }
            }
        }
    }
}

#[component]
fn ControllerMonitor(
    workspace: Workspace,
    controllers: Signal<Controllers>,
    route: Signal<Route>,
    mut attention: Signal<ThreadAttention>,
    notifications: Signal<bool>,
) -> Element {
    let workspace_id = workspace.workspace_id.clone();
    let workspace_name = workspace.name.clone();
    let workspace_for_status = workspace_id.clone();
    let _stream = use_hook(move || {
        connect_sse(
            &format!("/api/workspaces/{workspace_id}/controller/events"),
            move |data| {
                let Ok(message) = serde_json::from_str::<ControllerSubscriptionMessage>(&data)
                else {
                    return;
                };
                let status_update = match &message {
                    ControllerSubscriptionMessage::Operation {
                        operation: ControllerOperation::ThreadStatusUpdated { thread_id, status },
                    } => Some((*thread_id, *status)),
                    _ => None,
                };
                let mut states = controllers.write();
                let remote = states.entry(workspace_id.clone()).or_default();
                let previous = status_update.and_then(|(thread, _)| {
                    remote
                        .value
                        .as_ref()
                        .and_then(|controller| controller.thread_status(thread))
                });
                remote.apply(message);
                if let Some((thread_id, status)) = status_update {
                    if previous == Some(status)
                        || route.read().workspace.as_deref() == Some(&workspace_id)
                            && route.read().thread == Some(thread_id.0)
                    {
                        return;
                    }
                    let summary = match status {
                        AgentStatus::AwaitingApproval => "Approval required",
                        AgentStatus::AwaitingQuestion => "Questions require answers",
                        AgentStatus::Failed => "Turn failed",
                        AgentStatus::Completed => "Turn completed",
                        AgentStatus::Cancelled => "Turn cancelled",
                        _ => return,
                    };
                    let thread_name = remote
                        .value
                        .as_ref()
                        .and_then(|controller| {
                            controller
                                .threads()
                                .iter()
                                .find(|thread| thread.id == thread_id)
                        })
                        .map(thread_name)
                        .unwrap_or_else(|| format!("Thread {}", thread_id.0));
                    attention
                        .write()
                        .insert((workspace_id.clone(), thread_id.0), status);
                    if notifications()
                        && matches!(
                            status,
                            AgentStatus::AwaitingApproval | AgentStatus::AwaitingQuestion
                        )
                    {
                        notify(&format!("{workspace_name} · {thread_name} · {summary}"));
                    }
                }
            },
            move |connected| {
                if let Some(remote) = controllers.write().get_mut(&workspace_for_status) {
                    remote.connected = connected;
                }
            },
        )
    });
    rsx! {}
}

#[component]
fn ResizeHandle(
    side: &'static str,
    value: Signal<i32>,
    default_value: i32,
    min: i32,
    max: i32,
    storage_key: &'static str,
) -> Element {
    let mut dragging = use_signal(|| false);
    rsx! {
        div { class: "resize-handle",
            button {
                aria_label: "Resize {side}",
                title: "Drag or use arrow keys to resize; double-click to reset",
                onpointerdown: move |event| {
                    event.prevent_default();
                    dragging.set(true);
                },
                onclick: move |event| {
                    let delta = if event.modifiers().contains(Modifiers::SHIFT) { -24 } else { 24 };
                    let next = (value() + delta).clamp(min, max);
                    value.set(next);
                    storage_set(storage_key, &next.to_string());
                },
                onkeydown: move |event| {
                    let delta = match event.key() {
                        Key::ArrowLeft => if side == "Navigation" { -16 } else { 16 },
                        Key::ArrowRight => if side == "Navigation" { 16 } else { -16 },
                        _ => return,
                    };
                    event.prevent_default();
                    let next = (value() + delta).clamp(min, max);
                    value.set(next);
                    storage_set(storage_key, &next.to_string());
                },
                ondoubleclick: move |_| {
                    value.set(default_value);
                    storage_set(storage_key, &default_value.to_string());
                },
                "⋮"
            }
        }
        if dragging() {
            div {
                class: "resize-drag-overlay",
                onpointermove: move |event| {
                    let x = event.client_coordinates().x as i32;
                    let raw = if side == "Navigation" {
                        x
                    } else {
                        web_sys::window()
                            .and_then(|window| window.inner_width().ok())
                            .and_then(|width| width.as_f64())
                            .map(|width| width as i32 - x)
                            .unwrap_or(value())
                    };
                    let next = raw.clamp(min, max);
                    value.set(next);
                    storage_set(storage_key, &next.to_string());
                },
                onpointerup: move |_| dragging.set(false),
                onpointercancel: move |_| dragging.set(false),
            }
        }
    }
}

#[component]
fn Navigation(
    workspaces: Vec<Workspace>,
    controllers: Signal<Controllers>,
    route: Signal<Route>,
    pins: Signal<Vec<Pin>>,
    attention: Signal<ThreadAttention>,
    nav_open: Signal<bool>,
    nav_width: Signal<i32>,
    mobile_panel: Signal<MobilePanel>,
    theme: Signal<String>,
    notifications: Signal<bool>,
) -> Element {
    let mut collapsed = use_signal(|| storage_json::<HashSet<String>>(WORKSPACE_COLLAPSE_KEY));
    let mut dragging_pin = use_signal(|| None::<usize>);
    let selected_workspace = route.read().workspace.clone();
    use_effect(move || {
        if let Some(workspace) = route.read().workspace.clone()
            && collapsed.write().remove(&workspace)
        {
            save_json(WORKSPACE_COLLAPSE_KEY, &*collapsed.read());
        }
    });
    let mut sorted_workspaces = workspaces.clone();
    sorted_workspaces.sort_by_key(|workspace| workspace.name.to_lowercase());

    rsx! {
        aside {
            class: if mobile_panel() == MobilePanel::Navigation { "navigation drawer-open" } else { "navigation" },
            aria_label: "Navigation",
            div { class: "navigation-brand",
                h1 { "Atra" }
                button {
                    class: "icon-button mobile-only",
                    aria_label: "Close navigation",
                    onclick: move |_| mobile_panel.set(MobilePanel::None),
                    "×"
                }
            }
            section { class: "pinned-section",
                h2 { "Pinned Threads" }
                if pins.read().is_empty() {
                    p { class: "muted compact", "Threads you use appear here." }
                }
                for (index, pin) in pins.read().clone().into_iter().enumerate() {
                    if let Some((workspace_name, thread, status)) =
                        find_pin(&workspaces, &controllers.read(), &pin)
                    {
                        div {
                            class: "navigation-row pin-row",
                            draggable: "true",
                            ondragstart: move |_| dragging_pin.set(Some(index)),
                            ondragend: move |_| dragging_pin.set(None),
                            ondragover: move |event| event.prevent_default(),
                            ondrop: move |_| {
                                if let Some(from) = dragging_pin() {
                                    move_pin(pins, from, index);
                                }
                                dragging_pin.set(None);
                            },
                            button {
                                class: "navigation-link",
                                aria_current: if selected_workspace.as_deref() == Some(&pin.workspace)
                                    && route.read().thread == Some(pin.thread) { "page" } else { "false" },
                                onclick: {
                                    let workspace = pin.workspace.clone();
                                    move |_| {
                                        navigate(route, Route {
                                            workspace: Some(workspace.clone()),
                                            thread: Some(pin.thread),
                                            detail: None,
                                        });
                                        mobile_panel.set(MobilePanel::None);
                                    }
                                },
                                div { class: "thread-title-line",
                                    ThreadStatusIndicator {
                                        status,
                                        attention: attention
                                            .read()
                                            .get(&(pin.workspace.clone(), pin.thread))
                                            .copied(),
                                    }
                                    span { class: "row-title", "{thread_name(&thread)}" }
                                }
                                small { "{workspace_name}" }
                            }
                            PinButton {
                                pins,
                                workspace: pin.workspace.clone(),
                                thread: pin.thread,
                                name: thread_name(&thread),
                                pinned: true,
                            }
                        }
                    } else {
                        div { class: "navigation-row pin-row offline",
                            draggable: "true",
                            ondragstart: move |_| dragging_pin.set(Some(index)),
                            ondragend: move |_| dragging_pin.set(None),
                            ondragover: move |event| event.prevent_default(),
                            ondrop: move |_| {
                                if let Some(from) = dragging_pin() {
                                    move_pin(pins, from, index);
                                }
                                dragging_pin.set(None);
                            },
                            button {
                                class: "navigation-link",
                                onclick: {
                                    let workspace = pin.workspace.clone();
                                    move |_| navigate(route, Route {
                                        workspace: Some(workspace.clone()),
                                        thread: Some(pin.thread),
                                        detail: None,
                                    })
                                },
                                div { class: "thread-title-line",
                                    span { class: "row-title", "Thread {pin.thread}" }
                                }
                                small { "{pin.workspace} · Offline" }
                            }
                            PinButton {
                                pins,
                                workspace: pin.workspace.clone(),
                                thread: pin.thread,
                                name: format!("Thread {}", pin.thread),
                                pinned: true,
                            }
                        }
                    }
                }
            }
            nav { class: "workspace-tree", aria_label: "Workspaces",
                h2 { "Workspaces" }
                if sorted_workspaces.is_empty() {
                    p { class: "empty-copy", "No running Workspaces." }
                }
                for workspace in sorted_workspaces {
                    section { class: "workspace-group",
                        header {
                            button {
                                class: "workspace-toggle",
                                aria_expanded: !collapsed.read().contains(&workspace.workspace_id),
                                onclick: {
                                    let id = workspace.workspace_id.clone();
                                    move |_| {
                                        if !collapsed.write().insert(id.clone()) {
                                            collapsed.write().remove(&id);
                                        }
                                        save_json(WORKSPACE_COLLAPSE_KEY, &*collapsed.read());
                                    }
                                },
                                span { if collapsed.read().contains(&workspace.workspace_id) { "▸" } else { "▾" } }
                                strong { "{workspace.name}" }
                            }
                            NewThreadButton {
                                workspace: workspace.workspace_id.clone(),
                                connected: controllers
                                    .read()
                                    .get(&workspace.workspace_id)
                                    .is_some_and(|remote| remote.connected),
                                controller: controllers
                                    .read()
                                    .get(&workspace.workspace_id)
                                    .and_then(|remote| remote.value.clone()),
                                route,
                                pins,
                            }
                        }
                        if !collapsed.read().contains(&workspace.workspace_id) {
                            if let Some(controller) = controllers
                                .read()
                                .get(&workspace.workspace_id)
                                .and_then(|remote| remote.value.clone())
                            {
                                for thread in root_threads(&controller) {
                                    div { class: "navigation-row workspace-thread-row",
                                        button {
                                            class: "navigation-link root-thread",
                                            aria_current: if selected_workspace.as_deref() == Some(&workspace.workspace_id)
                                                && route.read().thread.is_some_and(|selected| {
                                                    root_id(controller.threads(), ThreadId(selected)) == thread.id
                                                }) { "page" } else { "false" },
                                            onclick: {
                                                let workspace_id = workspace.workspace_id.clone();
                                                move |_| {
                                                    navigate(route, Route {
                                                        workspace: Some(workspace_id.clone()),
                                                        thread: Some(thread.id.0),
                                                        detail: None,
                                                    });
                                                    mobile_panel.set(MobilePanel::None);
                                                }
                                            },
                                            div { class: "thread-title-line",
                                                ThreadStatusIndicator {
                                                    status: controller.thread_status(thread.id),
                                                    attention: attention
                                                        .read()
                                                        .get(&(workspace.workspace_id.clone(), thread.id.0))
                                                        .copied(),
                                                }
                                                span { class: "row-title", "{thread_name(&thread)}" }
                                            }
                                        }
                                        PinButton {
                                            pins,
                                            workspace: workspace.workspace_id.clone(),
                                            thread: thread.id.0,
                                            name: thread_name(&thread),
                                            pinned: pins.read().iter().any(|pin| {
                                                pin.workspace == workspace.workspace_id && pin.thread == thread.id.0
                                            }),
                                        }
                                    }
                                }
                            } else {
                                p { class: "muted compact", "Loading…" }
                            }
                        }
                    }
                }
            }
            details { class: "global-settings",
                summary { "Application settings" }
                label {
                    "Theme"
                    select {
                        value: "{theme}",
                        onchange: move |event| {
                            let value = event.value();
                            storage_set(THEME_KEY, &value);
                            theme.set(value);
                        },
                        option { value: "system", "System" }
                        option { value: "light", "Light" }
                        option { value: "dark", "Dark" }
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
                if web_sys::Notification::permission() == web_sys::NotificationPermission::Denied {
                    small { "Notifications are blocked by your browser." }
                }
                div { class: "button-row",
                    button {
                        onclick: move |_| {
                            nav_open.set(false);
                            storage_set(NAV_OPEN_KEY, "closed");
                        },
                        "Hide navigation"
                    }
                    button {
                        onclick: move |_| {
                            nav_width.set(288);
                            storage_set(NAV_WIDTH_KEY, "288");
                        },
                        "Reset width"
                    }
                }
            }
        }
    }
}

fn find_pin(
    workspaces: &[Workspace],
    controllers: &Controllers,
    pin: &Pin,
) -> Option<(String, atra_protocol::Thread, Option<AgentStatus>)> {
    let workspace = workspaces
        .iter()
        .find(|item| item.workspace_id == pin.workspace)?;
    let controller = controllers.get(&pin.workspace)?.value.as_ref()?;
    let thread = controller
        .threads()
        .iter()
        .find(|thread| thread.id.0 == pin.thread)?
        .clone();
    Some((
        workspace.name.clone(),
        thread.clone(),
        controller.thread_status(thread.id),
    ))
}

#[component]
fn ThreadStatusIndicator(status: Option<AgentStatus>, attention: Option<AgentStatus>) -> Element {
    if matches!(
        status,
        Some(AgentStatus::Running | AgentStatus::Compacting | AgentStatus::Cancelling)
    ) {
        return rsx! {
            span {
                class: "thread-status-indicator spinner",
                role: "img",
                aria_label: "{factual_status(status)}",
                title: "{factual_status(status)}",
            }
        };
    }

    let (class, label) = match attention {
        Some(AgentStatus::Completed) => ("completed", "Completed"),
        Some(AgentStatus::AwaitingQuestion) => ("question", "Question required"),
        Some(AgentStatus::AwaitingApproval) => ("approval", "Approval required"),
        Some(AgentStatus::Failed) => ("failed", "Failed"),
        Some(AgentStatus::Cancelled) => ("cancelled", "Cancelled"),
        _ => return rsx! {},
    };
    rsx! {
        span {
            class: "thread-status-indicator {class}",
            role: "img",
            aria_label: "{label}",
            title: "{label}",
        }
    }
}

fn move_pin(mut pins: Signal<Vec<Pin>>, from: usize, to: usize) {
    if from >= pins.read().len() || to >= pins.read().len() || from == to {
        return;
    }
    let pin = pins.write().remove(from);
    pins.write().insert(to, pin);
    save_json(PINS_KEY, &*pins.read());
}

fn set_pin(mut pins: Signal<Vec<Pin>>, workspace: &str, thread: i64, pinned: bool) {
    let index = pins
        .read()
        .iter()
        .position(|pin| pin.workspace == workspace && pin.thread == thread);
    match (index, pinned) {
        (None, true) => pins.write().insert(
            0,
            Pin {
                workspace: workspace.to_owned(),
                thread,
            },
        ),
        (Some(index), false) => {
            pins.write().remove(index);
        }
        _ => return,
    }
    save_json(PINS_KEY, &*pins.read());
}

#[component]
fn PinButton(
    pins: Signal<Vec<Pin>>,
    workspace: String,
    thread: i64,
    name: String,
    pinned: bool,
) -> Element {
    rsx! {
        button {
            class: if pinned { "icon-button pin-button pinned" } else { "icon-button pin-button" },
            aria_label: if pinned { "Unpin {name}" } else { "Pin {name}" },
            title: if pinned { "Unpin" } else { "Pin" },
            onclick: move |_| set_pin(pins, &workspace, thread, !pinned),
            svg {
                class: "pin-icon",
                view_box: "0 0 24 24",
                path { d: "M9 3h6l-1 6 3 3v2H7v-2l3-3-1-6Z" }
                path { d: "M12 14v7" }
            }
        }
    }
}

#[component]
fn NewThreadButton(
    workspace: String,
    connected: bool,
    controller: Option<ControllerState>,
    route: Signal<Route>,
    pins: Signal<Vec<Pin>>,
) -> Element {
    let mut pending = use_signal(|| false);
    let mut error = use_signal(String::new);
    rsx! {
        button {
            class: "icon-button",
            aria_label: "New Thread in {workspace}",
            disabled: !connected || pending(),
            title: if error().is_empty() { "New Thread" } else { "{error}" },
            onclick: move |_| {
                let workspace = workspace.clone();
                let controller = controller.clone();
                pending.set(true);
                spawn(async move {
                    match command(&workspace, &Command::ThreadCreate { display_name: None }).await {
                        Ok(CommandResult::ThreadCreated { thread_id }) => {
                            if let Some(controller) = controller.as_ref() {
                                ensure_pin(pins, &workspace, thread_id, controller);
                            } else {
                                pins.write().insert(0, Pin { workspace: workspace.clone(), thread: thread_id.0 });
                                save_json(PINS_KEY, &*pins.read());
                            }
                            navigate(route, Route {
                                workspace: Some(workspace),
                                thread: Some(thread_id.0),
                                detail: None,
                            });
                            error.set(String::new());
                        }
                        Ok(_) => error.set("Unexpected Controller response".to_owned()),
                        Err(message) => error.set(message),
                    }
                    pending.set(false);
                });
            },
            "+"
        }
        if !error().is_empty() {
            small { class: "inline-error", "{error}" }
        }
    }
}

#[component]
fn Landing(connected: bool) -> Element {
    rsx! {
        section { class: "landing",
            div { class: "landing-mark", "A" }
            h2 { "Choose a Workspace" }
            p {
                if connected {
                    "Select a Thread from Navigation, or create one in a Workspace."
                } else {
                    "Connecting to Atra…"
                }
            }
        }
    }
}

#[component]
fn MobileShellHeader(mobile_panel: Signal<MobilePanel>) -> Element {
    rsx! {
        header { class: "mobile-shell-header",
            button {
                class: "icon-button",
                aria_label: "Open navigation",
                onclick: move |_| open_mobile_navigation(mobile_panel),
                "☰"
            }
            strong { "Atra" }
        }
    }
}

#[component]
fn LoadingState(label: &'static str) -> Element {
    rsx! {
        section { class: "loading-state", aria_busy: "true",
            div { class: "skeleton wide" }
            div { class: "skeleton" }
            p { "{label}" }
        }
    }
}

#[component]
fn WorkspaceLanding(
    workspace: String,
    controller: ControllerState,
    connected: bool,
    route: Signal<Route>,
    pins: Signal<Vec<Pin>>,
) -> Element {
    rsx! {
        section { class: "landing",
            h2 { "Choose a Thread" }
            p { "Open a root Thread from Navigation or create a new one." }
            NewThreadButton { workspace, connected, controller: Some(controller), route, pins }
        }
    }
}

#[component]
fn ThreadPage(
    workspace: Workspace,
    thread: i64,
    detail: Option<Detail>,
    controller: ControllerState,
    controller_connected: bool,
    route: Signal<Route>,
    pins: Signal<Vec<Pin>>,
    nav_open: Signal<bool>,
    utility_open: Signal<bool>,
    utility_tab: Signal<UtilityTab>,
    utility_width: Signal<i32>,
    mobile_panel: Signal<MobilePanel>,
    modes: Signal<HashMap<String, TranscriptMode>>,
    scroll_positions: Signal<HashMap<String, i32>>,
) -> Element {
    let store = ThreadStore::new(use_store(RemoteState::<ThreadState>::default));
    let mut error = use_signal(String::new);
    let dialog = use_signal(|| None::<DialogState>);
    let selected_activity = use_signal(|| None::<(TurnKey, ActivityKey)>);
    let workspace_id = workspace.workspace_id.clone();
    let workspace_for_stream = workspace_id.clone();
    let _thread_stream = use_hook(move || {
        connect_sse(
            &format!("/api/workspaces/{workspace_for_stream}/threads/{thread}/events"),
            move |data| match serde_json::from_str::<ThreadSubscriptionMessage>(&data) {
                Ok(message) => store.apply(message),
                Err(parse_error) => error.set(parse_error.to_string()),
            },
            move |connected| store.set_connected(connected),
        )
    });
    let connection = store.read_connection();
    let connected = controller_connected && connection.connected();
    let connection_message = connection
        .terminal()
        .map(str::to_owned)
        .unwrap_or_else(|| "The live connection was interrupted.".to_owned());
    let metadata = store.read_metadata();
    let has_state = metadata.is_loaded();
    let checkpoint_metadata = metadata.metadata().cloned();
    let selected = controller
        .threads()
        .iter()
        .find(|item| item.id.0 == thread)
        .cloned();
    let name = selected
        .as_ref()
        .map(thread_name)
        .unwrap_or_else(|| format!("Thread {thread}"));
    let status = selected
        .as_ref()
        .and_then(|selected| controller.thread_status(selected.id));
    let mode_key = format!("{}:{thread}", workspace.workspace_id);
    let mode = modes
        .read()
        .get(&mode_key)
        .copied()
        .unwrap_or(TranscriptMode::Pretty);
    let selected_detail = detail.clone();

    use_effect({
        let workspace_name = workspace.name.clone();
        let name = name.clone();
        move || set_document_title(&workspace_name, &name)
    });
    use_effect(move || {
        if let Some(detail) = detail.clone() {
            utility_open.set(true);
            let tab = match detail {
                Detail::Checkpoint(_) => UtilityTab::Checkpoints,
                Detail::Process { .. } => UtilityTab::Processes,
            };
            utility_tab.set(tab);
            storage_set(UTILITY_OPEN_KEY, "open");
            storage_set(UTILITY_TAB_KEY, &format!("{tab:?}").to_lowercase());
        }
    });
    use_effect({
        let selected_activity = selected_activity.clone();
        move || {
            if selected_activity().is_some() {
                utility_tab.set(UtilityTab::Activity);
                utility_open.set(true);
                storage_set(UTILITY_OPEN_KEY, "open");
                storage_set(UTILITY_TAB_KEY, "activity");
            }
        }
    });

    rsx! {
        ContextHeader {
            workspace: workspace.clone(),
            name: name.clone(),
            status,
            connected,
            mode,
            mode_key: mode_key.clone(),
            modes,
            route,
            thread,
            nav_open,
            utility_open,
            utility_tab,
            mobile_panel,
            controller: controller.clone(),
            pins,
            error,
        }
        if !connected {
            div { class: "connection-banner", role: "status",
                strong { "Read only — reconnecting" }
                span { "{connection_message}" }
                button { onclick: move |_| {
                    if let Some(window) = web_sys::window() { let _ = window.location().reload(); }
                }, "Retry" }
            }
        }
        if !error().is_empty() {
            div { class: "toast error-toast", role: "alert",
                span { "{error}" }
                button { aria_label: "Dismiss error", onclick: move |_| error.set(String::new()), "×" }
            }
        }
        if has_state {
            if let Some(Detail::Checkpoint(checkpoint)) = selected_detail {
                CheckpointPreview {
                    workspace: workspace_id.clone(),
                    thread,
                    checkpoint,
                    metadata: checkpoint_metadata.expect("loaded thread has metadata"),
                    connected,
                    mode,
                    route,
                    dialog,
                    error,
                    scroll_positions,
                    selected_activity,
                }
            } else {
                Transcript {
                    store,
                    mode,
                    workspace: workspace_id.clone(),
                    thread,
                    dialog,
                    error,
                    scroll_positions,
                    selected_activity,
                }
                Composer {
                    workspace: workspace_id.clone(),
                    thread,
                    store,
                    controller: controller.clone(),
                    connected,
                    pins,
                    error,
                }
            }
            UtilityPanel {
                workspace: workspace_id.clone(),
                thread,
                store,
                controller: controller.clone(),
                connected,
                route,
                utility_open,
                utility_tab,
                mobile_panel,
                dialog,
                error,
                selected_activity,
            }
            if utility_open() && !is_narrow_viewport() {
                ResizeHandle {
                    side: "Utility",
                    value: utility_width,
                    default_value: 352,
                    min: 272,
                    max: 520,
                    storage_key: UTILITY_WIDTH_KEY,
                }
            }
        } else {
            LoadingState { label: "Loading Thread…" }
        }
        if let Some(dialog_state) = dialog.read().clone() {
            AppDialog {
                workspace: workspace_id,
                thread,
                state: dialog_state,
                controller,
                connected,
                dialog,
                route,
                pins,
                error,
            }
        }
    }
}

#[component]
fn ContextHeader(
    workspace: Workspace,
    name: String,
    status: Option<AgentStatus>,
    connected: bool,
    mode: TranscriptMode,
    mode_key: String,
    modes: Signal<HashMap<String, TranscriptMode>>,
    route: Signal<Route>,
    thread: i64,
    nav_open: Signal<bool>,
    utility_open: Signal<bool>,
    utility_tab: Signal<UtilityTab>,
    mobile_panel: Signal<MobilePanel>,
    controller: ControllerState,
    pins: Signal<Vec<Pin>>,
    error: Signal<String>,
) -> Element {
    rsx! {
        header { class: "context-header",
            button {
                class: "icon-button",
                aria_label: "Toggle navigation",
                onclick: move |_| {
                    if is_narrow_viewport() {
                        nav_open.set(true);
                        storage_set(NAV_OPEN_KEY, "open");
                        if mobile_panel() == MobilePanel::Navigation {
                            mobile_panel.set(MobilePanel::None);
                        } else {
                            mobile_panel.set(MobilePanel::Navigation);
                        }
                    } else {
                        mobile_panel.set(MobilePanel::None);
                        let next = !nav_open();
                        nav_open.set(next);
                        storage_set(NAV_OPEN_KEY, if next { "open" } else { "closed" });
                    }
                },
                "☰"
            }
            div { class: "context-title",
                small { "{workspace.name}" }
                strong { "{name}" }
            }
            div { class: "context-status", role: "status",
                span { class: "status-dot" }
                span { if connected { "{factual_status(status)}" } else { "Reconnecting" } }
            }
            div { class: "mode-switch desktop-only", role: "group", aria_label: "Transcript mode",
                button {
                    class: if mode == TranscriptMode::Pretty { "selected" } else { "" },
                    aria_pressed: mode == TranscriptMode::Pretty,
                    onclick: {
                        let key = mode_key.clone();
                        move |_| { modes.write().insert(key.clone(), TranscriptMode::Pretty); }
                    },
                    "Pretty"
                }
                button {
                    class: if mode == TranscriptMode::Raw { "selected" } else { "" },
                    aria_pressed: mode == TranscriptMode::Raw,
                    onclick: {
                        let key = mode_key.clone();
                        move |_| { modes.write().insert(key.clone(), TranscriptMode::Raw); }
                    },
                    "Raw"
                }
            }
            details { class: "header-menu",
                summary { aria_label: "Thread and display actions", "•••" }
                button {
                    class: "mobile-only",
                    onclick: {
                        let key = mode_key.clone();
                        move |_| { modes.write().insert(key.clone(), TranscriptMode::Pretty); }
                    },
                    "Pretty"
                }
                button {
                    class: "mobile-only",
                    onclick: {
                        let key = mode_key.clone();
                        move |_| { modes.write().insert(key.clone(), TranscriptMode::Raw); }
                    },
                    "Raw"
                }
                button {
                    onclick: move |_| {
                        utility_tab.set(UtilityTab::Thread);
                        utility_open.set(true);
                        storage_set(UTILITY_TAB_KEY, "thread");
                        storage_set(UTILITY_OPEN_KEY, "open");
                    },
                    "Thread settings"
                }
                HeaderActionButton {
                    workspace: workspace.workspace_id.clone(),
                    thread,
                    action: HeaderThreadAction::Continue,
                    connected: connected && !matches!(status, Some(AgentStatus::Running | AgentStatus::Compacting | AgentStatus::Cancelling)),
                    controller: controller.clone(),
                    pins,
                    error,
                }
                HeaderActionButton {
                    workspace: workspace.workspace_id.clone(),
                    thread,
                    action: HeaderThreadAction::Compact,
                    connected: connected && !matches!(status, Some(AgentStatus::Running | AgentStatus::Compacting | AgentStatus::Cancelling)),
                    controller: controller.clone(),
                    pins,
                    error,
                }
                HeaderActionButton {
                    workspace: workspace.workspace_id.clone(),
                    thread,
                    action: HeaderThreadAction::Checkpoint,
                    connected,
                    controller: controller.clone(),
                    pins,
                    error,
                }
            }
            button {
                class: "icon-button",
                aria_label: "Toggle utility panel",
                onclick: move |_| {
                    if is_narrow_viewport() {
                        utility_open.set(true);
                        storage_set(UTILITY_OPEN_KEY, "open");
                        if mobile_panel() == MobilePanel::Utility {
                            mobile_panel.set(MobilePanel::None);
                        } else {
                            mobile_panel.set(MobilePanel::Utility);
                        }
                    } else {
                        mobile_panel.set(MobilePanel::None);
                        let next = !utility_open();
                        utility_open.set(next);
                        storage_set(UTILITY_OPEN_KEY, if next { "open" } else { "closed" });
                    }
                },
                "◫"
            }
            if route.read().detail.is_some() {
                button {
                    class: "live-link",
                    onclick: {
                        let workspace = workspace.workspace_id.clone();
                        move |_| navigate(route, Route {
                            workspace: Some(workspace.clone()),
                            thread: Some(thread),
                            detail: None,
                        })
                    },
                    "Live"
                }
            }
        }
    }
}

#[component]
fn HeaderActionButton(
    workspace: String,
    thread: i64,
    action: HeaderThreadAction,
    connected: bool,
    controller: ControllerState,
    pins: Signal<Vec<Pin>>,
    error: Signal<String>,
) -> Element {
    let mut pending = use_signal(|| false);
    let label = match action {
        HeaderThreadAction::Continue => "Continue",
        HeaderThreadAction::Compact => "Compact history",
        HeaderThreadAction::Checkpoint => "Create Checkpoint",
    };
    rsx! {
        button {
            disabled: !connected || pending(),
            onclick: move |_| {
                pending.set(true);
                let workspace = workspace.clone();
                let controller = controller.clone();
                spawn(async move {
                    let next = match action {
                        HeaderThreadAction::Continue => Command::ThreadContinue {
                            thread_id: ThreadId(thread),
                            allow_questions: true,
                        },
                        HeaderThreadAction::Compact => Command::ThreadCompact {
                            thread_id: ThreadId(thread),
                            allow_questions: true,
                        },
                        HeaderThreadAction::Checkpoint => Command::ThreadCheckpointCreate {
                            thread_id: ThreadId(thread),
                        },
                    };
                    match command(&workspace, &next).await {
                        Ok(_) => {
                            ensure_pin(pins, &workspace, ThreadId(thread), &controller);
                            error.set(String::new());
                        }
                        Err(message) => error.set(message),
                    }
                    pending.set(false);
                });
            },
            if pending() { "Working…" } else { "{label}" }
        }
    }
}

#[component]
fn Transcript(
    store: ThreadStore,
    mode: TranscriptMode,
    workspace: String,
    thread: i64,
    dialog: Signal<Option<DialogState>>,
    error: Signal<String>,
    scroll_positions: Signal<HashMap<String, i32>>,
    selected_activity: Signal<Option<(TurnKey, ActivityKey)>>,
) -> Element {
    let mut following = use_signal(|| true);
    let mut has_latest = use_signal(|| false);
    let mut restored_mode = use_signal(|| None::<TranscriptMode>);
    let remote = if mode == TranscriptMode::Pretty {
        store.read_transcript_structure()
    } else {
        store.read_raw_transcript()
    };
    if !remote.is_loaded() {
        return rsx! { LoadingState { label: "Loading Transcript…" } };
    }
    let turns = remote.turn_keys();
    let raw_keys = (mode == TranscriptMode::Raw).then(|| remote.raw_keys());
    let turns_empty = turns.is_empty();
    let scroll_key = format!("{workspace}:{thread}:{mode:?}");

    use_effect(use_reactive((&mode,), {
        let key = scroll_key.clone();
        move |(mode,)| {
            if mode == TranscriptMode::Pretty {
                store.track_pretty_transcript_content();
            } else {
                store.track_raw_transcript_content();
            }
            if let Some(window) = web_sys::window()
                && let Some(document) = window.document()
                && let Some(element) = document.get_element_by_id("transcript-scroll")
            {
                if restored_mode.peek().as_ref() != Some(&mode) {
                    if let Some(saved) = scroll_positions.peek().get(&key).copied() {
                        element.set_scroll_top(saved);
                        following.set(transcript_is_near_bottom(
                            element.scroll_height(),
                            element.scroll_top(),
                            element.client_height(),
                        ));
                    } else {
                        element.set_scroll_top(element.scroll_height());
                        following.set(true);
                    }
                    has_latest.set(false);
                    restored_mode.set(Some(mode));
                } else if *following.peek() {
                    element.set_scroll_top(element.scroll_height());
                    has_latest.set(false);
                } else {
                    has_latest.set(true);
                }
            }
        }
    }));

    rsx! {
        main {
            id: "transcript-scroll",
            class: "transcript transcript-scroll",
            aria_live: "polite",
            onscroll: {
                let key = scroll_key.clone();
                move |_| {
                    if let Some(window) = web_sys::window()
                        && let Some(document) = window.document()
                        && let Some(element) = document.get_element_by_id("transcript-scroll")
                    {
                        scroll_positions.write().insert(key.clone(), element.scroll_top());
                        let near_bottom = transcript_is_near_bottom(
                            element.scroll_height(),
                            element.scroll_top(),
                            element.client_height(),
                        );
                        following.set(near_bottom);
                        if near_bottom {
                            has_latest.set(false);
                        }
                    }
                }
            },
            div { class: "reading-column",
                if mode == TranscriptMode::Pretty {
                    if turns_empty {
                        section { class: "empty-transcript",
                            h2 { "No conversation yet" }
                            p { "Use the composer below to start this Thread." }
                        }
                    }
                    for turn_key in turns {
                        TurnCard {
                            key: "{turn_key}",
                            store,
                            turn_key,
                            dialog,
                            selected_activity,
                        }
                    }
                    InteractionPanel {
                        store,
                        workspace: workspace.clone(),
                        error,
                    }
                } else if let Some(raw_keys) = raw_keys {
                    section { class: "raw-events",
                        for (index, raw_key) in raw_keys.into_iter().enumerate() {
                            RawEventRow {
                                key: "{raw_key}",
                                store,
                                raw_key,
                                index,
                            }
                        }
                    }
                }
            }
        }
        if has_latest() {
            button {
                class: "latest-button",
                r#type: "button",
                onclick: {
                    let key = scroll_key.clone();
                    move |_| {
                        if let Some(window) = web_sys::window()
                            && let Some(document) = window.document()
                            && let Some(element) = document.get_element_by_id("transcript-scroll")
                        {
                            element.set_scroll_top(element.scroll_height());
                            scroll_positions
                                .write()
                                .insert(key.clone(), element.scroll_top());
                        }
                        following.set(true);
                        has_latest.set(false);
                    }
                },
                "Latest"
            }
        }
    }
}

#[component]
fn TurnCard(
    store: ThreadStore,
    turn_key: TurnKey,
    dialog: Signal<Option<DialogState>>,
    selected_activity: Signal<Option<(TurnKey, ActivityKey)>>,
) -> Element {
    let mut group_open = use_signal(|| false);
    let mut activity_auto_expanded = use_signal(|| false);
    let connection = store.read_connection();
    let connected = connection.connected();
    let turn_read = store.read_turn(turn_key);
    let Some(turn) = turn_read.value() else {
        return rsx! {};
    };
    let prompt_html = render_markdown(turn.prompt());
    let prompt_sequence = turn.prompt_sequence();
    let answer = turn
        .answer()
        .map(|(sequence, answer)| (sequence, render_markdown(answer)));
    let outcome = turn.outcome().cloned();
    let active = turn.is_active();
    let activities = turn.activity_keys();

    if active && !*activity_auto_expanded.peek() {
        activity_auto_expanded.set(true);
        group_open.set(true);
    }

    let summary = turn_read.activity_summary(&activities);
    rsx! {
        article {
            class: if active { "turn turn-card active-turn" } else { "turn turn-card" },
            section { class: "turn-message user-message",
                div { class: "speaker-label", "You" }
                div {
                    class: "turn-prose markdown",
                    dangerous_inner_html: "{prompt_html}",
                }
                div { class: "turn-actions",
                    TurnCopyButton { store, turn_key, target: TurnCopyTarget::Prompt }
                    if let Some(sequence) = prompt_sequence {
                        button {
                            disabled: active || !connected,
                            onclick: {
                                move |_| dialog.set(Some(DialogState::Rewind {
                                    checkpoint: None,
                                    sequence,
                                }))
                            },
                            "Rewind"
                        }
                    }
                }
            }
            if !activities.is_empty() {
                section { class: "activity-group",
                    if group_open() {
                        div { class: "collapsible-expanded",
                            button {
                                class: "collapse-bar",
                                aria_label: "Collapse activities",
                                onclick: move |_| group_open.set(false),
                            }
                            div { class: "activity-list",
                                for activity_key in activities {
                                    ActivityRow {
                                        key: "{activity_key}",
                                        store,
                                        turn_key,
                                        activity_key,
                                        selected_activity,
                                    }
                                }
                            }
                        }
                    } else {
                        button {
                            class: "collapsible-compact activity-group-compact",
                            onclick: move |_| group_open.set(true),
                            span { "{summary}" }
                        }
                    }
                }
            }
            if let Some((sequence, answer_html)) = answer {
                section { class: "turn-message assistant-message",
                    div { class: "speaker-label", "Atra" }
                    div {
                        class: "turn-prose markdown",
                        dangerous_inner_html: "{answer_html}",
                    }
                    div { class: "turn-actions",
                        TurnCopyButton { store, turn_key, target: TurnCopyTarget::Answer }
                        if let Some(sequence) = sequence {
                            button {
                                disabled: active || !connected,
                                onclick: {
                                    move |_| dialog.set(Some(DialogState::Rewind {
                                        checkpoint: None,
                                        sequence,
                                    }))
                                },
                                "Rewind"
                            }
                        }
                    }
                }
            }
            if let Some(outcome) = outcome {
                if matches!(outcome, atra_protocol::TurnOutcome::Cancelled) {
                    div { class: "turn-outcome cancelled", "Turn cancelled" }
                }
                if let atra_protocol::TurnOutcome::Failed { message } = outcome {
                    {
                        let summary = message.lines().next().unwrap_or("Turn failed");
                        rsx! {
                            details { class: "turn-outcome failed",
                                summary { "Failed · {summary}" }
                                pre { "{message}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn RawEventRow(store: ThreadStore, raw_key: RawKey, index: usize) -> Element {
    let item = store.read_raw_item(raw_key);
    let Some(json) = item.value() else {
        return rsx! {};
    };
    rsx! {
        pre {
            "data-event-index": "{index}",
            "{json}"
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TurnCopyTarget {
    Prompt,
    Answer,
}

#[component]
fn TurnCopyButton(store: ThreadStore, turn_key: TurnKey, target: TurnCopyTarget) -> Element {
    let mut copied = use_signal(|| false);
    let mut generation = use_signal(|| 0_u64);
    let aria_label = match target {
        TurnCopyTarget::Prompt => "Copy prompt",
        TurnCopyTarget::Answer => "Copy response",
    };

    rsx! {
        button {
            aria_label,
            onclick: move |_| {
                let value = {
                    let turn = store.peek_turn(turn_key);
                    turn.value().and_then(|turn| match target {
                        TurnCopyTarget::Prompt => Some(turn.prompt().to_owned()),
                        TurnCopyTarget::Answer => {
                            turn.answer().map(|(_, answer)| answer.to_owned())
                        }
                    })
                };
                let Some(value) = value else {
                    return;
                };
                let current = generation.peek().wrapping_add(1);
                generation.set(current);
                spawn(async move {
                    if !copy_text(value).await {
                        if *generation.peek() == current {
                            copied.set(false);
                        }
                        return;
                    }
                    if *generation.peek() != current {
                        return;
                    }
                    copied.set(true);
                    TimeoutFuture::new(2_000).await;
                    if *generation.peek() == current {
                        copied.set(false);
                    }
                });
            },
            if copied() { "Copied" } else { "Copy" }
        }
    }
}

#[component]
fn ActivityRow(
    store: ThreadStore,
    turn_key: TurnKey,
    activity_key: ActivityKey,
    selected_activity: Signal<Option<(TurnKey, ActivityKey)>>,
) -> Element {
    let activity_read = store.read_activity(turn_key, activity_key.clone());
    let Some(activity) = activity_read.value() else {
        return rsx! {};
    };
    match activity {
        ActivityDisplay::Commentary { markdown } => rsx! {
            article {
                class: "activity-commentary markdown",
                dangerous_inner_html: "{render_markdown(markdown)}",
            }
        },
        ActivityDisplay::Todo { items } => {
            let completed = items
                .iter()
                .filter(|item| matches!(item.status, atra_protocol::TodoStatus::Completed))
                .count();
            let current = items
                .iter()
                .find(|item| matches!(item.status, atra_protocol::TodoStatus::InProgress))
                .or_else(|| {
                    items
                        .iter()
                        .find(|item| matches!(item.status, atra_protocol::TodoStatus::Pending))
                })
                .map(|item| item.step.as_str())
                .unwrap_or("Plan complete");
            let compact = format!("{current} · {completed}/{}", items.len());
            rsx! {
                button {
                    class: "collapsible-compact activity-todo",
                    onclick: move |_| selected_activity.set(Some((turn_key, activity_key.clone()))),
                    span { "{compact}" }
                }
            }
        }
        ActivityDisplay::Reasoning { summary, .. } => {
            let headline = summary.lines().next().unwrap_or("Thinking…").to_owned();
            rsx! {
                button {
                    class: "collapsible-compact activity-reasoning",
                    onclick: move |_| selected_activity.set(Some((turn_key, activity_key.clone()))),
                    span { "{headline}" }
                }
            }
        }
        ActivityDisplay::Command(display) => rsx! {
            button {
                class: if display.active {
                    "collapsible-compact activity-command running"
                } else {
                    "collapsible-compact activity-command"
                },
                onclick: move |_| selected_activity.set(Some((turn_key, activity_key.clone()))),
                span { class: "command-summary-text", "{display.summary}" }
            }
        },
        ActivityDisplay::Search { summary, .. } => rsx! {
            button {
                class: "collapsible-compact activity-search",
                onclick: move |_| selected_activity.set(Some((turn_key, activity_key.clone()))),
                span { "{summary}" }
            }
        },
        ActivityDisplay::Question { summary, .. } => rsx! {
            button {
                class: "collapsible-compact activity-question",
                onclick: move |_| selected_activity.set(Some((turn_key, activity_key.clone()))),
                span { "{summary}" }
            }
        },
        ActivityDisplay::Approval { allowed, reason } => rsx! {
            div {
                class: if allowed { "activity-approval allowed" } else { "activity-approval denied" },
                if allowed {
                    "Allowed"
                } else if let Some(reason) = reason {
                    "Denied — {reason}"
                } else {
                    "Denied"
                }
            }
        },
        ActivityDisplay::Retry {
            summary,
            current,
            max,
        } => rsx! {
            div { class: "activity-retry", "{summary} · attempt {current}/{max}" }
        },
        ActivityDisplay::Skill { name, path } => rsx! {
            div { class: "activity-skill",
                strong { "{name}" }
                code { "{path}" }
            }
        },
        ActivityDisplay::Compaction => rsx! {
            div { class: "activity-compaction", "Compacting conversation history…" }
        },
        ActivityDisplay::Boundary => rsx! {
            div { class: "activity-boundary", "Older context was compacted." }
        },
        ActivityDisplay::Failure { message } => {
            let headline = message.lines().next().unwrap_or("Failed");
            rsx! {
                details { class: "activity-failure",
                    summary { "{headline}" }
                    pre { "{message}" }
                }
            }
        }
        ActivityDisplay::Cancelled => rsx! {
            div { class: "activity-cancelled", "Cancelled" }
        },
        ActivityDisplay::Unsupported { summary } => rsx! {
            div { class: "activity-unsupported", "{summary}" }
        },
    }
}

#[component]
fn ActivityDetail(
    store: ThreadStore,
    selected_activity: Signal<Option<(TurnKey, ActivityKey)>>,
) -> Element {
    let Some((turn_key, activity_key)) = selected_activity.read().clone() else {
        return rsx! {
            div { class: "activity-detail-placeholder",
                "Select an activity in the transcript to inspect it here."
            }
        };
    };
    let activity_read = store.read_activity(turn_key, activity_key);
    let Some(activity) = activity_read.value() else {
        return rsx! {
            div { class: "activity-detail-placeholder",
                "This activity is no longer available."
            }
        };
    };
    match activity {
        ActivityDisplay::Commentary { markdown } => rsx! {
            div {
                class: "activity-commentary markdown",
                dangerous_inner_html: "{render_markdown(markdown)}",
            }
        },
        ActivityDisplay::Todo { items } => {
            let completed = items
                .iter()
                .filter(|item| matches!(item.status, atra_protocol::TodoStatus::Completed))
                .count();
            rsx! {
                div { class: "activity-todo",
                    div { class: "todo-progress",
                        span { "{completed}/{items.len()}" }
                        progress { value: completed as f64, max: items.len() as f64 }
                    }
                    ul {
                        for item in items {
                            li {
                                class: match item.status {
                                    atra_protocol::TodoStatus::Completed => "completed",
                                    atra_protocol::TodoStatus::InProgress => "in-progress",
                                    atra_protocol::TodoStatus::Pending => "pending",
                                },
                                span { class: "todo-state", aria_hidden: "true" }
                                span { "{item.step}" }
                            }
                        }
                    }
                }
            }
        }
        ActivityDisplay::Reasoning { summary, .. } => rsx! {
            div { class: "activity-reasoning",
                div {
                    class: "reasoning-detail markdown",
                    dangerous_inner_html: "{render_markdown(&summary)}",
                }
            }
        },
        ActivityDisplay::Command(display) => rsx! {
            CommandOperations { display }
        },
        ActivityDisplay::Search { detail, .. } => rsx! {
            div { class: "activity-search",
                if !detail.is_empty() {
                    pre { class: "search-detail", "{detail}" }
                }
            }
        },
        ActivityDisplay::Question { detail, .. } => rsx! {
            div { class: "activity-question",
                if !detail.is_empty() {
                    pre { class: "question-detail", "{detail}" }
                }
            }
        },
        ActivityDisplay::Approval { allowed, reason } => rsx! {
            div {
                class: if allowed { "activity-approval allowed" } else { "activity-approval denied" },
                if allowed {
                    "Allowed"
                } else if let Some(reason) = reason {
                    "Denied — {reason}"
                } else {
                    "Denied"
                }
            }
        },
        ActivityDisplay::Retry {
            summary,
            current,
            max,
        } => rsx! {
            div { class: "activity-retry", "{summary} · attempt {current}/{max}" }
        },
        ActivityDisplay::Skill { name, path } => rsx! {
            div { class: "activity-skill",
                strong { "{name}" }
                code { "{path}" }
            }
        },
        ActivityDisplay::Compaction => rsx! {
            div { class: "activity-compaction", "Compacting conversation history…" }
        },
        ActivityDisplay::Boundary => rsx! {
            div { class: "activity-boundary", "Older context was compacted." }
        },
        ActivityDisplay::Failure { message } => {
            let headline = message.lines().next().unwrap_or("Failed");
            rsx! {
                details { class: "activity-failure",
                    summary { "{headline}" }
                    pre { "{message}" }
                }
            }
        }
        ActivityDisplay::Cancelled => rsx! {
            div { class: "activity-cancelled", "Cancelled" }
        },
        ActivityDisplay::Unsupported { summary } => rsx! {
            div { class: "activity-unsupported", "{summary}" }
        },
    }
}

#[component]
fn CommandOperations(display: CommandDisplay) -> Element {
    rsx! {
        div { class: "command-operations",
            for approval in &display.approvals {
                div {
                    class: if approval.allowed { "command-approval allowed" } else { "command-approval denied" },
                    if approval.allowed {
                        "Allowed"
                    } else if let Some(reason) = &approval.reason {
                        "Denied — {reason}"
                    } else {
                        "Denied"
                    }
                }
            }
            for operation in &display.operations {
                section { class: "command-operation",
                    header {
                        code { "{operation.runner}" }
                        span { "{operation.status}" }
                    }
                    if !operation.command.is_empty() {
                        pre {
                            class: "command-source highlighted",
                            dangerous_inner_html: "{highlight(&operation.command, \"bash\")}",
                        }
                    }
                    if !operation.output.is_empty() {
                        pre { class: "command-output", "{operation.output}" }
                    }
                    if operation.omitted_bytes > 0 {
                        div { class: "command-omitted",
                            "… {operation.omitted_bytes} output bytes omitted"
                        }
                    }
                    for diff in &operation.diffs {
                        div {
                            class: "command-diff highlighted diff-view",
                            dangerous_inner_html: "{highlight(diff, \"diff\")}",
                        }
                    }
                }
            }
            if display.masked {
                div { class: "command-masked", "Model context uses masked output." }
            }
        }
    }
}

#[component]
fn Composer(
    workspace: String,
    thread: i64,
    store: ThreadStore,
    controller: ControllerState,
    connected: bool,
    pins: Signal<Vec<Pin>>,
    error: Signal<String>,
) -> Element {
    let active_turn = store.read_active_turn();
    let metadata_read = store.read_metadata();
    let diagnostics_read = store.read_diagnostics();
    if !active_turn.is_loaded() {
        return rsx! {};
    }
    let mut draft = use_signal(|| storage_get(&draft_key(&workspace, thread)));
    let mut pending = use_signal(|| false);
    let mut history_index = use_signal(|| None::<usize>);
    let mut saved_draft = use_signal(String::new);
    let mut composing = use_signal(|| false);
    let history = storage_json::<Vec<String>>(&history_key(&workspace, thread));
    let active = active_turn.is_active();
    let awaiting = active_turn.is_awaiting_interaction();
    let diagnostics = diagnostics_read.value().unwrap_or_default();
    let Some(metadata) = metadata_read.metadata().cloned() else {
        return rsx! {};
    };
    let workspace_for_input = workspace.clone();
    let workspace_for_submit = workspace.clone();

    rsx! {
        div { class: "composer-region",
            if awaiting {
                button {
                    class: "attention-bar",
                    onclick: move |_| {
                        if let Some(element) = web_sys::window()
                            .and_then(|window| window.document())
                            .and_then(|document| document.query_selector(".interaction").ok().flatten())
                        {
                            element.scroll_into_view();
                        }
                    },
                    "Input is required in the Transcript ↑"
                }
            }
            form {
                id: "composer",
                class: "composer",
                onsubmit: move |event| {
                    event.prevent_default();
                    let message = draft();
                    if message.trim().is_empty() || active || awaiting || pending() {
                        return;
                    }
                    let workspace = workspace_for_submit.clone();
                    let controller = controller.clone();
                    pending.set(true);
                    spawn(async move {
                        match command(&workspace, &Command::ThreadSend {
                            thread_id: ThreadId(thread),
                            message: message.clone(),
                            allow_questions: true,
                        }).await {
                            Ok(_) => {
                                save_sent_message(&workspace, thread, &message);
                                storage_set(&draft_key(&workspace, thread), "");
                                draft.set(String::new());
                                history_index.set(None);
                                ensure_pin(pins, &workspace, ThreadId(thread), &controller);
                                error.set(String::new());
                            }
                            Err(message) => error.set(message),
                        }
                        pending.set(false);
                    });
                },
                div { class: "composer-box",
                label { class: "sr-only", r#for: "message", "Message" }
                textarea {
                    id: "message",
                    aria_label: "Message",
                    placeholder: "Message Atra…",
                    value: "{draft}",
                    oncompositionstart: move |_| composing.set(true),
                    oncompositionend: move |_| composing.set(false),
                    oninput: move |event| {
                        let value = event.value();
                        storage_set(&draft_key(&workspace_for_input, thread), &value);
                        draft.set(value);
                        history_index.set(None);
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
                                .and_then(|element| element.dyn_into::<HtmlFormElement>().ok())
                            {
                                let _ = form.request_submit();
                            }
                            return;
                        }
                        if composing() || !matches!(event.key(), Key::ArrowUp | Key::ArrowDown) {
                            return;
                        }
                        let Some(textarea) = web_sys::window()
                            .and_then(|window| window.document())
                            .and_then(|document| document.get_element_by_id("message"))
                            .and_then(|element| element.dyn_into::<HtmlTextAreaElement>().ok())
                        else {
                            return;
                        };
                        let start = textarea.selection_start().ok().flatten().unwrap_or(0) as usize;
                        let end = textarea.selection_end().ok().flatten().unwrap_or(0) as usize;
                        if start != end {
                            return;
                        }
                        let value = draft();
                        let start = byte_index_at_utf16_offset(&value, start);
                        let at_first = !value[..start].contains('\n');
                        let at_last = !value[start..].contains('\n');
                        if event.key() == Key::ArrowUp && at_first && !history.is_empty() {
                            event.prevent_default();
                            let next = history_index().map(|index| index.saturating_sub(1)).unwrap_or_else(|| {
                                saved_draft.set(value);
                                history.len() - 1
                            });
                            history_index.set(Some(next));
                            draft.set(history[next].clone());
                        } else if event.key() == Key::ArrowDown
                            && at_last
                            && let Some(index) = history_index()
                        {
                            event.prevent_default();
                            if index + 1 < history.len() {
                                history_index.set(Some(index + 1));
                                draft.set(history[index + 1].clone());
                            } else {
                                history_index.set(None);
                                draft.set(saved_draft());
                            }
                        }
                    }
                }
                if active {
                    button {
                        class: "primary stop-button",
                        r#type: "button",
                        disabled: !connected || pending(),
                        onclick: {
                            let workspace = workspace.clone();
                            move |_| {
                                pending.set(true);
                                let workspace = workspace.clone();
                                spawn(async move {
                                    if let Err(message) = command(&workspace, &Command::ThreadCancel {
                                        thread_id: ThreadId(thread),
                                    }).await {
                                        error.set(message);
                                    }
                                    pending.set(false);
                                });
                            }
                        },
                        "Stop"
                    }
                } else {
                    button {
                        class: "primary",
                        r#type: "submit",
                        disabled: !connected || awaiting || pending() || draft().trim().is_empty(),
                        if pending() { "Sending…" } else { "Send" }
                    }
                }
                }
                div { class: "composer-status",
                    span { class: "composer-model", "{metadata.model} ({metadata.reasoning_effort})" }
                    span { title: "{diagnostics.usage_raw.clone().unwrap_or_default()}", "{diagnostics.composer_context}" }
                    span { title: "{diagnostics.usage_raw.clone().unwrap_or_default()}", "{diagnostics.composer_cache}" }
                    for summary in diagnostics.composer_quotas {
                        span { title: "{diagnostics.limits_raw.clone().unwrap_or_default()}", "{summary}" }
                    }
                }
            }
        }
    }
}

#[component]
fn InteractionPanel(store: ThreadStore, workspace: String, error: Signal<String>) -> Element {
    let remote = store.read_interaction();
    let Some(interaction) = remote.value() else {
        return rsx! {};
    };
    match interaction {
        atra_protocol::PendingInteraction::Approval(approval) => rsx! {
            ApprovalForm {
                key: "approval-{approval.id().0}",
                store,
                workspace,
                id: approval.id(),
                error,
            }
        },
        atra_protocol::PendingInteraction::Questions(request) => rsx! {
            QuestionForm {
                key: "questions-{request.id.0}",
                store,
                workspace,
                id: request.id,
                error,
            }
        },
    }
}

#[component]
fn ApprovalForm(
    store: ThreadStore,
    workspace: String,
    id: InteractionId,
    error: Signal<String>,
) -> Element {
    let remote = store.read_interaction();
    let Some(approval) = remote
        .value()
        .and_then(|interaction| match interaction {
            PendingInteraction::Approval(approval) => Some(approval),
            PendingInteraction::Questions(_) => None,
        })
        .filter(|approval| approval.id() == id)
    else {
        return rsx! {};
    };
    let tool = approval.tool();
    let operation = approval.operation_label().unwrap_or("Approval");
    let arguments = serde_json::to_string_pretty(approval.arguments()).unwrap_or_default();
    let connected = store.read_connection().connected();
    let mut reason = use_signal(String::new);
    let mut pending = use_signal(|| false);
    rsx! {
        section { class: "interaction", role: "group", aria_label: "Approval required",
            header {
                div {
                    h3 { "Approval required" }
                    p { "{operation} · {tool}" }
                }
            }
            details {
                summary { "Complete arguments" }
                pre { "{arguments}" }
            }
            label {
                "Denial reason (optional)"
                textarea { value: "{reason}", oninput: move |event| reason.set(event.value()) }
            }
            div { class: "equal-actions",
                button {
                    disabled: !connected || pending(),
                    onclick: {
                        let workspace = workspace.clone();
                        move |_| {
                            pending.set(true);
                            let workspace = workspace.clone();
                            spawn(async move {
                                if let Err(message) = command(&workspace, &Command::ApprovalAllow { approval_id: id }).await {
                                    error.set(message);
                                } else {
                                    error.set(String::new());
                                }
                                pending.set(false);
                            });
                        }
                    },
                    "Allow"
                }
                button {
                    disabled: !connected || pending(),
                    onclick: move |_| {
                        pending.set(true);
                        let workspace = workspace.clone();
                        let reason = reason();
                        spawn(async move {
                            if let Err(message) = command(&workspace, &Command::ApprovalDeny {
                                approval_id: id,
                                reason: if reason.trim().is_empty() { None } else { Some(reason) },
                            }).await {
                                error.set(message);
                            } else {
                                error.set(String::new());
                            }
                            pending.set(false);
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
    store: ThreadStore,
    workspace: String,
    id: InteractionId,
    error: Signal<String>,
) -> Element {
    let remote = store.read_interaction();
    let Some(request) = remote
        .value()
        .and_then(|interaction| match interaction {
            PendingInteraction::Questions(request) => Some(request),
            PendingInteraction::Approval(_) => None,
        })
        .filter(|request| request.id == id)
    else {
        return rsx! {};
    };
    let request_id = request.id;
    let question_count = request.questions.len();
    let connected = store.read_connection().connected();
    let mut pending = use_signal(|| false);
    let mut answers = use_signal(move || {
        (0..question_count)
            .map(|_| QuestionAnswer {
                selected_option: None,
                note: String::new(),
            })
            .collect::<Vec<_>>()
    });
    let mut touched = use_signal(move || vec![false; question_count]);
    rsx! {
        form {
            class: "interaction question-form",
            onsubmit: move |event| {
                event.prevent_default();
                pending.set(true);
                let workspace = workspace.clone();
                let values = answers();
                spawn(async move {
                    if let Err(message) = command(&workspace, &Command::QuestionAnswer {
                        request_id,
                        answers: values,
                    }).await {
                        error.set(message);
                    } else {
                        error.set(String::new());
                    }
                    pending.set(false);
                });
            },
            h3 { "Questions" }
            for (index, question) in request.questions.iter().enumerate() {
                fieldset {
                    legend { "{question.question}" }
                    for option in question.options.iter() {
                        label { class: "radio-card",
                            input {
                                r#type: "radio",
                                name: "question-{index}",
                                value: "{option.label}",
                                checked: answers.read()[index].selected_option.as_deref() == Some(&option.label),
                                onchange: {
                                    let label = option.label.clone();
                                    move |_| {
                                        answers.write()[index].selected_option = Some(label.clone());
                                        touched.write()[index] = true;
                                    }
                                },
                            }
                            span {
                                strong { "{option.label}" }
                                if question.recommended_options.contains(&option.label) {
                                    small { class: "recommended", "Recommended" }
                                }
                                small { "{option.description}" }
                            }
                        }
                    }
                    label { class: "radio-card",
                        input {
                            r#type: "radio",
                            name: "question-{index}",
                            value: "",
                            checked: touched.read()[index] && answers.read()[index].selected_option.is_none(),
                            onchange: move |_| {
                                answers.write()[index].selected_option = None;
                                touched.write()[index] = true;
                            },
                        }
                        span {
                            strong { "None of these" }
                            small { "Provide another answer in the note if useful." }
                        }
                    }
                    label {
                        "Optional note"
                        textarea {
                            value: "{answers.read()[index].note}",
                            oninput: move |event| answers.write()[index].note = event.value(),
                        }
                    }
                }
            }
            button {
                class: "primary",
                r#type: "submit",
                disabled: !connected || pending() || touched.read().iter().any(|selected| !selected),
                if pending() { "Submitting…" } else { "Submit answers" }
            }
        }
    }
}

#[component]
fn UtilityPanel(
    workspace: String,
    thread: i64,
    store: ThreadStore,
    controller: ControllerState,
    connected: bool,
    route: Signal<Route>,
    utility_open: Signal<bool>,
    utility_tab: Signal<UtilityTab>,
    mobile_panel: Signal<MobilePanel>,
    dialog: Signal<Option<DialogState>>,
    error: Signal<String>,
    selected_activity: Signal<Option<(TurnKey, ActivityKey)>>,
) -> Element {
    rsx! {
        aside {
            class: if mobile_panel() == MobilePanel::Utility { "utility drawer-open" } else { "utility" },
            aria_label: "Utility panel",
            header { class: "utility-header",
                h2 { "{utility_tab().label()}" }
                button {
                    class: "icon-button",
                    aria_label: "Close utility panel",
                    onclick: move |_| {
                        utility_open.set(false);
                        mobile_panel.set(MobilePanel::None);
                        storage_set(UTILITY_OPEN_KEY, "closed");
                    },
                    "×"
                }
            }
            div { class: "utility-tabs", role: "tablist",
                for tab in [UtilityTab::Thread, UtilityTab::Activity, UtilityTab::Children, UtilityTab::Checkpoints, UtilityTab::Processes] {
                    button {
                        role: "tab",
                        aria_selected: utility_tab() == tab,
                        class: if utility_tab() == tab { "selected" } else { "" },
                        onclick: move |_| {
                            utility_tab.set(tab);
                            storage_set(UTILITY_TAB_KEY, &format!("{tab:?}").to_lowercase());
                        },
                        "{tab.label()}"
                    }
                }
            }
            div { class: "utility-content",
                match utility_tab() {
                    UtilityTab::Thread => rsx! {
                        ThreadUtility {
                            workspace: workspace.clone(),
                            thread,
                            store,
                            controller: controller.clone(),
                            connected,
                            dialog,
                            error,
                        }
                    },
                    UtilityTab::Activity => rsx! {
                        ActivityDetail {
                            store,
                            selected_activity,
                        }
                    },
                    UtilityTab::Children => rsx! {
                        ChildrenUtility { workspace: workspace.clone(), thread, controller: controller.clone(), route }
                    },
                    UtilityTab::Checkpoints => rsx! {
                        CheckpointsUtility { workspace: workspace.clone(), thread, store, route }
                    },
                    UtilityTab::Processes => rsx! {
                        ProcessesUtility {
                            workspace: workspace.clone(),
                            thread,
                            store,
                            focused: route.read().detail.clone(),
                            connected,
                            route,
                            dialog,
                            error,
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn ThreadUtility(
    workspace: String,
    thread: i64,
    store: ThreadStore,
    controller: ControllerState,
    connected: bool,
    dialog: Signal<Option<DialogState>>,
    error: Signal<String>,
) -> Element {
    let metadata_read = store.read_metadata();
    let diagnostics_read = store.read_diagnostics();
    let Some(metadata) = metadata_read.metadata().cloned() else {
        return rsx! {};
    };
    let mut selected_model = use_signal(|| format!("{}\n{}", metadata.provider, metadata.model));
    let mut reasoning = use_signal(|| metadata.reasoning_effort.clone());
    let mut pending = use_signal(|| false);
    let models = controller
        .providers()
        .iter()
        .flat_map(|provider| provider.models().iter().cloned())
        .collect::<Vec<_>>();
    let efforts = models
        .iter()
        .find(|model| format!("{}\n{}", model.provider, model.id) == selected_model())
        .map(|model| model.supported_reasoning_efforts.clone())
        .unwrap_or_else(|| vec![reasoning()]);
    let diagnostics = diagnostics_read.value().unwrap_or_default();
    let rename_metadata = metadata.clone();
    let delete_metadata = metadata.clone();
    rsx! {
        section { class: "utility-section",
            h3 { "Identity" }
            dl { class: "metadata-list",
                dt { "Name" } dd { "{thread_name(&metadata)}" }
                dt { "Provider" } dd { "{metadata.provider}" }
                dt { "Model" } dd { "{metadata.model}" }
                dt { "Effort" } dd { "{metadata.reasoning_effort}" }
            }
            button {
                disabled: !connected,
                onclick: move |_| dialog.set(Some(DialogState::Rename { current: thread_name(&rename_metadata) })),
                "Rename Thread"
            }
        }
        if !models.is_empty() {
            form {
                class: "utility-section",
                onsubmit: move |event| {
                    event.prevent_default();
                    let Some((provider, model)) = selected_model()
                        .split_once('\n')
                        .map(|(provider, model)| (provider.to_owned(), model.to_owned()))
                    else { return; };
                    pending.set(true);
                    let workspace = workspace.clone();
                    let effort = reasoning();
                    spawn(async move {
                        if let Err(message) = command(&workspace, &Command::ThreadSetModel {
                            thread_id: ThreadId(thread),
                            provider,
                            model,
                            reasoning_effort: effort,
                        }).await {
                            error.set(message);
                        } else {
                            error.set(String::new());
                        }
                        pending.set(false);
                    });
                },
                h3 { "Model" }
                label {
                    "Provider and model"
                    select {
                        value: "{selected_model}",
                        onchange: move |event| selected_model.set(event.value()),
                        for model in &models {
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
                button { r#type: "submit", disabled: !connected || pending(), "Apply model" }
            }
        }
        if diagnostics.usage_raw.is_some() || diagnostics.limits_raw.is_some() {
            section { class: "utility-section",
                h3 { "Diagnostics" }
                if let Some(summary) = diagnostics.token_summary.clone() {
                    p { class: "diagnostic-summary", "{summary}" }
                }
                if let Some(summary) = diagnostics.context_summary.clone() {
                    p { class: "diagnostic-summary", "{summary}" }
                }
                if let Some(summary) = diagnostics.cache_summary.clone() {
                    p { class: "diagnostic-summary", "{summary}" }
                }
                for summary in diagnostics.quota_windows.clone() {
                    p { class: "diagnostic-summary", "{summary}" }
                }
                if let Some(value) = diagnostics.usage_raw.clone() {
                    details { summary { "Latest token usage" } pre { "{value}" } }
                }
                if let Some(value) = diagnostics.limits_raw.clone() {
                    details { summary { "Rate limits" } pre { "{value}" } }
                }
            }
        }
        section { class: "utility-section danger-zone",
            h3 { "Danger zone" }
            p { "Deleting removes this Thread and all descendants." }
            button {
                class: "danger",
                disabled: !connected,
                onclick: move |_| dialog.set(Some(DialogState::Delete { name: thread_name(&delete_metadata) })),
                "Delete Thread"
            }
        }
    }
}

#[component]
fn ChildrenUtility(
    workspace: String,
    thread: i64,
    controller: ControllerState,
    route: Signal<Route>,
) -> Element {
    let family = family_threads(&controller, ThreadId(thread));
    rsx! {
        section { class: "family-tree",
            if family.len() <= 1 {
                p { class: "empty-copy", "This Thread has no children." }
            }
            for item in family {
                button {
                    class: if item.id.0 == thread { "family-row current" } else { "family-row" },
                    aria_current: if item.id.0 == thread { "page" } else { "false" },
                    style: if item.parent_thread_id.is_some() { "margin-inline-start: 1.25rem" } else { "" },
                    onclick: {
                        let workspace = workspace.clone();
                        move |_| navigate(route, Route {
                            workspace: Some(workspace.clone()),
                            thread: Some(item.id.0),
                            detail: None,
                        })
                    },
                    strong { "{thread_name(&item)}" }
                    small { "{factual_status(controller.thread_status(item.id))}" }
                }
            }
        }
    }
}

#[component]
fn CheckpointsUtility(
    workspace: String,
    thread: i64,
    store: ThreadStore,
    route: Signal<Route>,
) -> Element {
    let remote = store.read_checkpoints();
    let Some(checkpoints) = remote.value() else {
        return rsx! {};
    };
    rsx! {
        section {
            if checkpoints.is_empty() {
                p { class: "empty-copy", "No Checkpoints yet. Create one from Thread actions." }
            }
            for checkpoint in checkpoints {
                button {
                    class: "checkpoint-row",
                    aria_current: if route.read().detail == Some(Detail::Checkpoint(checkpoint.id.0)) { "page" } else { "false" },
                    onclick: {
                        let workspace = workspace.clone();
                        let id = checkpoint.id.0;
                        move |_| navigate(route, Route {
                            workspace: Some(workspace.clone()),
                            thread: Some(thread),
                            detail: Some(Detail::Checkpoint(id)),
                        })
                    },
                    strong { "Checkpoint {checkpoint.id.0}" }
                    span { "{checkpoint.reason}" }
                    small { "{checkpoint.created_at_ms}" }
                }
            }
        }
    }
}

#[component]
fn CheckpointPreview(
    workspace: String,
    thread: i64,
    checkpoint: i64,
    metadata: atra_protocol::Thread,
    connected: bool,
    mode: TranscriptMode,
    route: Signal<Route>,
    dialog: Signal<Option<DialogState>>,
    error: Signal<String>,
    scroll_positions: Signal<HashMap<String, i32>>,
    selected_activity: Signal<Option<(TurnKey, ActivityKey)>>,
) -> Element {
    let store = ThreadStore::new(use_store(RemoteState::<ThreadState>::default));
    let mut checkpoint_metadata = use_signal(|| None::<atra_protocol::ThreadCheckpoint>);
    let workspace_for_stream = workspace.clone();
    let _stream = use_hook(move || {
        connect_sse(
            &format!(
                "/api/workspaces/{workspace_for_stream}/threads/{thread}/checkpoints/{checkpoint}/events"
            ),
            move |data| match serde_json::from_str::<CheckpointSubscriptionMessage>(&data) {
                Ok(CheckpointSubscriptionMessage::Snapshot { state }) => {
                    let (checkpoint, events) = state.into_parts();
                    match ThreadState::materialize(metadata.clone(), events, Vec::new(), Vec::new())
                    {
                        Ok(state) => {
                            checkpoint_metadata.set(Some(checkpoint));
                            store.apply(ThreadSubscriptionMessage::Snapshot { state });
                        }
                        Err(materialize_error) => error.set(materialize_error.to_string()),
                    }
                }
                Ok(CheckpointSubscriptionMessage::Terminal { terminal }) => {
                    store.apply(ThreadSubscriptionMessage::Terminal { terminal });
                }
                Err(parse_error) => error.set(parse_error.to_string()),
            },
            move |connected| store.set_connected(connected),
        )
    });
    let snapshot = store.read_snapshot();
    let loaded = snapshot.is_loaded();
    let sequence = snapshot.last_event_sequence();

    rsx! {
        section { class: "checkpoint-preview-header",
            div {
                small { "Checkpoint preview" }
                h2 { "Checkpoint {checkpoint}" }
                if let Some(state) = checkpoint_metadata.as_ref() { p { "{state.reason}" } }
            }
            if let Some(sequence) = sequence {
                div { class: "button-row",
                    button {
                        class: "primary",
                        disabled: !connected,
                        onclick: move |_| dialog.set(Some(DialogState::Fork {
                            checkpoint: Some(checkpoint),
                            sequence,
                        })),
                        "Fork"
                    }
                    button {
                        class: "warning",
                        disabled: !connected,
                        onclick: move |_| dialog.set(Some(DialogState::Restore { checkpoint })),
                        "Restore"
                    }
                    button {
                        class: "warning",
                        disabled: !connected,
                        onclick: move |_| dialog.set(Some(DialogState::Rewind {
                            checkpoint: Some(checkpoint),
                            sequence,
                        })),
                        "Rewind"
                    }
                }
            }
        }
        if loaded {
            Transcript {
                store,
                mode,
                workspace: workspace.clone(),
                thread,
                dialog,
                error,
                scroll_positions,
                selected_activity,
            }
        } else {
            LoadingState { label: "Loading Checkpoint…" }
        }
        div { class: "preview-bar",
            span { "Viewing a read-only Checkpoint. Composer is unavailable." }
            button {
                onclick: move |_| navigate(route, Route {
                    workspace: Some(workspace.clone()),
                    thread: Some(thread),
                    detail: None,
                }),
                "Return to live"
            }
        }
    }
}

#[component]
fn ProcessesUtility(
    workspace: String,
    thread: i64,
    store: ThreadStore,
    focused: Option<Detail>,
    connected: bool,
    route: Signal<Route>,
    dialog: Signal<Option<DialogState>>,
    error: Signal<String>,
) -> Element {
    let remote = store.read_processes();
    let Some(processes) = remote.value() else {
        return rsx! {};
    };
    let mut expanded = use_signal(HashSet::<String>::new);
    let focused_for_effect = focused.clone();
    use_effect(move || {
        if let Some(Detail::Process { runner, process }) = focused_for_effect.clone() {
            expanded.write().insert(format!("{runner}:{process}"));
        }
    });
    let mut groups = HashMap::<String, Vec<_>>::new();
    for process in processes {
        groups
            .entry(process.locator().runner().to_owned())
            .or_default()
            .push(process);
    }
    let mut runners = groups.into_iter().collect::<Vec<_>>();
    runners.sort_by(|left, right| left.0.cmp(&right.0));
    rsx! {
        section {
            if runners.is_empty() {
                p { class: "empty-copy", "No managed Processes for this Thread." }
            }
            for (runner, mut processes) in runners {
                {
                    processes.sort_by(|left, right| {
                        let left_running = matches!(left.status(), ProcessStatus::Running);
                        let right_running = matches!(right.status(), ProcessStatus::Running);
                        right_running.cmp(&left_running).then_with(|| right.locator().process_id().0.cmp(&left.locator().process_id().0))
                    });
                    rsx! {
                        section { class: "process-group",
                            h3 { "{runner}" }
                            for process in processes {
                                {
                                    let process_id = process.locator().process_id().0.clone();
                                    let key = format!("{runner}:{process_id}");
                                    let open = expanded.read().contains(&key);
                                    let detail_process_id = process_id.clone();
                                    rsx! {
                                        article { class: if focused == Some(Detail::Process { runner: runner.clone(), process: process_id.clone() }) { "process-row focused" } else { "process-row" },
                                            button {
                                                class: "process-summary",
                                                aria_expanded: open,
                                                onclick: {
                                                    let key = key.clone();
                                                    let workspace = workspace.clone();
                                                    let runner = runner.clone();
                                                    let process_id = process_id.clone();
                                                    move |_| {
                                                        if !expanded.write().insert(key.clone()) {
                                                            expanded.write().remove(&key);
                                                        } else {
                                                            navigate(route, Route {
                                                                workspace: Some(workspace.clone()),
                                                                thread: Some(thread),
                                                                detail: Some(Detail::Process {
                                                                    runner: runner.clone(),
                                                                    process: process_id.clone(),
                                                                }),
                                                            });
                                                        }
                                                    }
                                                },
                                                code { "{process.command()}" }
                                                small { "{process.status():?}" }
                                            }
                                            if open {
                                                ProcessDetail {
                                                    workspace: workspace.clone(),
                                                    thread,
                                                    runner: runner.clone(),
                                                    process: detail_process_id,
                                                    connected,
                                                    dialog,
                                                    error,
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ProcessDetail(
    workspace: String,
    thread: i64,
    runner: String,
    process: String,
    connected: bool,
    dialog: Signal<Option<DialogState>>,
    error: Signal<String>,
) -> Element {
    let mut remote = use_signal(RemoteState::<ProcessState>::default);
    let workspace_for_stream = workspace.clone();
    let runner_for_stream = runner.clone();
    let process_for_stream = process.clone();
    let output_id = format!("process-output-{runner}-{process}");
    let output_for_effect = output_id.clone();
    let _stream = use_hook(move || {
        connect_sse(
            &format!(
                "/api/workspaces/{workspace_for_stream}/runners/{runner_for_stream}/processes/{process_for_stream}/events?thread_id={thread}"
            ),
            move |data| match serde_json::from_str::<ProcessSubscriptionMessage>(&data) {
                Ok(message) => remote.write().apply(message),
                Err(parse_error) => error.set(parse_error.to_string()),
            },
            move |connected| remote.write().connected = connected,
        )
    });
    let output_len = remote
        .read()
        .value
        .as_ref()
        .map(|state| state.output_tail().len())
        .unwrap_or(0);
    use_effect(move || {
        let _ = output_len;
        if let Some(element) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id(&output_for_effect))
        {
            let distance = element.scroll_height() - element.scroll_top() - element.client_height();
            if distance < 80 {
                element.set_scroll_top(element.scroll_height());
            }
        }
    });
    rsx! {
        div { class: "process-detail",
            if let Some(state) = remote.read().value.clone() {
                if state.omitted_bytes() > 0 {
                    small { "{state.omitted_bytes()} earlier bytes omitted" }
                }
                pre { id: "{output_id}", class: "process-output", "{state.output_tail()}" }
                if matches!(state.process().status(), ProcessStatus::Running) {
                    button {
                        class: "warning",
                        disabled: !connected,
                        onclick: move |_| dialog.set(Some(DialogState::StopProcess {
                            runner: runner.clone(),
                            process: process.clone(),
                        })),
                        "Stop"
                    }
                }
            } else {
                p { class: "muted", "Loading output…" }
            }
        }
    }
}

#[component]
fn AppDialog(
    workspace: String,
    thread: i64,
    state: DialogState,
    controller: ControllerState,
    connected: bool,
    dialog: Signal<Option<DialogState>>,
    route: Signal<Route>,
    pins: Signal<Vec<Pin>>,
    error: Signal<String>,
) -> Element {
    let initial = match &state {
        DialogState::Rename { current } => current.clone(),
        _ => String::new(),
    };
    let mut value = use_signal(|| initial);
    let mut pending = use_signal(|| false);
    use_effect(move || {
        if let Some(element) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id("atra-dialog"))
            .and_then(|element| element.dyn_into::<HtmlDialogElement>().ok())
        {
            let _ = element.show_modal();
        }
    });
    let (title, description, label, severity, submit) = match &state {
        DialogState::Rename { .. } => (
            "Rename Thread",
            "Choose a clear name for this Thread.",
            Some("Thread name"),
            "",
            "Rename",
        ),
        DialogState::Fork { .. } => (
            "Fork from here",
            "Create a new child Thread from the selected point.",
            Some("New Thread name (optional)"),
            "",
            "Fork",
        ),
        DialogState::Rewind { .. } => (
            "Replace Thread history?",
            "Atra first saves the current history as a Checkpoint, then replaces it with the selected point.",
            None,
            "warning",
            "Rewind",
        ),
        DialogState::Restore { .. } => (
            "Restore Checkpoint?",
            "Atra first saves the current history as a Checkpoint, then restores the selected Checkpoint.",
            None,
            "warning",
            "Restore",
        ),
        DialogState::Delete { .. } => (
            "Delete Thread?",
            "This removes the Thread and all descendants. This action is destructive.",
            None,
            "danger",
            "Delete",
        ),
        DialogState::StopProcess { .. } => (
            "Stop Process?",
            "Request immediate termination of this managed Process.",
            None,
            "warning",
            "Stop",
        ),
    };
    rsx! {
        dialog {
            id: "atra-dialog",
            class: "app-dialog",
            aria_labelledby: "dialog-title",
            oncancel: move |event| {
                event.prevent_default();
                if !pending() {
                    dialog.set(None);
                }
            },
                h2 { id: "dialog-title", "{title}" }
                p { "{description}" }
                if let Some(label) = label {
                    label {
                        "{label}"
                        input {
                            autofocus: true,
                            value: "{value}",
                            oninput: move |event| value.set(event.value()),
                        }
                    }
                }
                div { class: "dialog-actions",
                    button {
                        onclick: move |_| dialog.set(None),
                        disabled: pending(),
                        "Cancel"
                    }
                    button {
                        class: "{severity}",
                        disabled: !connected || pending() || matches!(state, DialogState::Rename { .. }) && value().trim().is_empty(),
                        onclick: {
                            let workspace = workspace.clone();
                            let state = state.clone();
                            let controller = controller.clone();
                            move |_| {
                                pending.set(true);
                                let workspace = workspace.clone();
                                let state = state.clone();
                                let controller = controller.clone();
                                let name = value().trim().to_owned();
                                spawn(async move {
                                    let action = state.clone();
                                    let result = match &state {
                                        DialogState::Rename { .. } => command(&workspace, &Command::ThreadRename {
                                            thread_id: ThreadId(thread),
                                            display_name: name,
                                        }).await.map(|_| None),
                                        DialogState::Fork { checkpoint, sequence } => command(&workspace, &Command::ThreadFork {
                                            thread_id: ThreadId(thread),
                                            checkpoint_id: checkpoint.map(CheckpointId),
                                            sequence: *sequence,
                                            display_name: if name.is_empty() { None } else { Some(name) },
                                        }).await.map(|result| match result {
                                            CommandResult::ThreadForked { thread_id } => Some(thread_id),
                                            _ => None,
                                        }),
                                        DialogState::Rewind { checkpoint, sequence } => command(&workspace, &Command::ThreadReplaceHistory {
                                            thread_id: ThreadId(thread),
                                            target: HistoryTarget::Message {
                                                checkpoint_id: checkpoint.map(CheckpointId),
                                                sequence: *sequence,
                                            },
                                        }).await.map(|_| None),
                                        DialogState::Restore { checkpoint } => command(&workspace, &Command::ThreadReplaceHistory {
                                            thread_id: ThreadId(thread),
                                            target: HistoryTarget::Checkpoint { checkpoint_id: CheckpointId(*checkpoint) },
                                        }).await.map(|_| None),
                                        DialogState::Delete { .. } => command(&workspace, &Command::ThreadDeleteRecursive {
                                            thread_id: ThreadId(thread),
                                        }).await.map(|_| None),
                                        DialogState::StopProcess { runner, process } => command(&workspace, &Command::StopProcess {
                                            process: atra_protocol::ProcessLocator::new(
                                                ThreadId(thread),
                                                runner.clone(),
                                                ProcessId(process.clone()),
                                            ),
                                        }).await.map(|_| None),
                                    };
                                    match result {
                                        Ok(forked) => {
                                            error.set(String::new());
                                            dialog.set(None);
                                            match action {
                                                DialogState::Fork { .. } => {
                                                    if let Some(thread_id) = forked {
                                                        ensure_pin(pins, &workspace, ThreadId(thread), &controller);
                                                        navigate(route, Route {
                                                            workspace: Some(workspace),
                                                            thread: Some(thread_id.0),
                                                            detail: None,
                                                        });
                                                    }
                                                }
                                                DialogState::Delete { .. } => {
                                                    let root = root_id(controller.threads(), ThreadId(thread)).0;
                                                    if root == thread {
                                                        pins.write().retain(|pin| !(pin.workspace == workspace && pin.thread == root));
                                                        save_json(PINS_KEY, &*pins.read());
                                                    }
                                                    navigate(route, Route {
                                                        workspace: Some(workspace),
                                                        thread: None,
                                                        detail: None,
                                                    });
                                                }
                                                DialogState::Restore { .. } | DialogState::Rewind { .. } => navigate(route, Route {
                                                    workspace: Some(workspace),
                                                    thread: Some(thread),
                                                    detail: None,
                                                }),
                                                DialogState::Rename { .. } | DialogState::StopProcess { .. } => {}
                                            }
                                        }
                                        Err(message) => error.set(message),
                                    }
                                    pending.set(false);
                                });
                            }
                        },
                        if pending() { "Working…" } else { "{submit}" }
                    }
                }
        }
    }
}

fn transcript_is_near_bottom(scroll_height: i32, scroll_top: i32, client_height: i32) -> bool {
    scroll_height - scroll_top - client_height <= 80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_follows_when_viewport_is_near_the_bottom() {
        assert!(transcript_is_near_bottom(1_000, 325, 600));
        assert!(transcript_is_near_bottom(1_000, 320, 600));
    }

    #[test]
    fn transcript_stops_following_when_viewport_moves_away_from_the_bottom() {
        assert!(!transcript_is_near_bottom(1_000, 319, 600));
    }
}
