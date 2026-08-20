use std::{cell::RefCell, rc::Rc};

use atra_protocol::{
    ControllerState, FileContent, GitDiff, GitDiffScope, GitFileKind, GitFileStatus, ProcessStatus,
    Query, QueryError, QueryResponse, QueryResult, RunnerLifecycle, RunnerQuery,
};
use dioxus::dioxus_core::current_scope_id;
use dioxus::prelude::*;
use gloo_net::http::Request;
use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::Event;

use crate::{
    changes_state::{ChangesDiff, ChangesSettings, DiffCaches, RequestState},
    diff_view::{
        DiffPreferences, DiffViewFile, DiffViewHunk, DiffViewKind, DiffViewLine, DiffViewLineKind,
        DiffViewStatus, ReactiveDiffFile, ReactiveDiffViewer, file_anchor_id,
    },
    storage_set,
    thread_store::ThreadStore,
};

const DEFAULT_CONTEXT_LINES: u32 = 3;
const EXPANDED_CONTEXT_LINES: u32 = 23;
const FULL_CONTEXT_LINES: u32 = u32::MAX;
const FILE_PREVIEW_LINES: u32 = 20_000;

#[derive(Clone, Copy)]
struct ChangesState {
    owner: ScopeId,
    settings_key: Signal<String>,
    workspace: Signal<String>,
    runner: Signal<String>,
    directory_input: Signal<String>,
    directory: Signal<String>,
    scope: Signal<GitDiffScope>,
    base: Signal<String>,
    base_input: Signal<String>,
    ignore_whitespace: Signal<bool>,
    repository: Signal<Option<atra_protocol::RepositoryInfo>>,
    caches: Signal<DiffCaches>,
    requests: Signal<RequestState>,
    message: Signal<String>,
    file_view: Signal<Option<FileContent>>,
    initialized: Signal<bool>,
}

impl ChangesState {
    fn persist(self) {
        ChangesSettings {
            runner: (self.runner)(),
            directory: (self.directory)(),
            scope: (self.scope)(),
            base: (self.base)(),
            ignore_whitespace: (self.ignore_whitespace)(),
        }
        .save(&(self.settings_key)());
    }

    fn invalidate_scope(mut self, scope: GitDiffScope) {
        self.caches.write().mark_stale(scope);
        self.requests.write().invalidate_diff(scope);
    }

    fn invalidate_diff_options(mut self) {
        self.caches.write().mark_all_stale();
        self.requests.write().invalidate_diffs();
    }

    fn initialize(mut self) {
        if (self.initialized)() || (self.runner)().is_empty() {
            return;
        }
        self.initialized.set(true);
        self.persist();
        self.load_repository((self.scope)());
    }

    fn apply_directory(mut self) {
        self.directory.set((self.directory_input)());
        self.caches.set(DiffCaches::default());
        self.persist();
        self.load_repository((self.scope)());
    }

    fn select_runner(mut self, runner: String) {
        self.runner.set(runner);
        self.caches.set(DiffCaches::default());
        self.persist();
        self.load_repository((self.scope)());
    }

    fn select_scope(mut self, scope: GitDiffScope) {
        self.scope.set(scope);
        self.persist();
        if self.caches.read().needs_load(scope) {
            self.load_scope(scope, None, DEFAULT_CONTEXT_LINES);
        }
    }

    fn apply_base(mut self) {
        self.base.set((self.base_input)());
        self.invalidate_scope(GitDiffScope::Base);
        self.persist();
        self.load_scope(GitDiffScope::Base, None, DEFAULT_CONTEXT_LINES);
    }

    fn set_ignore_whitespace(mut self, ignore: bool) {
        self.ignore_whitespace.set(ignore);
        self.invalidate_diff_options();
        self.persist();
        self.load_scope((self.scope)(), None, DEFAULT_CONTEXT_LINES);
    }

    fn refresh(self) {
        let scope = (self.scope)();
        self.invalidate_scope(scope);
        self.load_scope(scope, None, DEFAULT_CONTEXT_LINES);
    }

    fn external_change(mut self) {
        self.caches.write().mark_all_except_stale((self.scope)());
        self.load_scope((self.scope)(), None, DEFAULT_CONTEXT_LINES);
    }

    fn load_repository(mut self, selected_scope: GitDiffScope) {
        let request_token = self.requests.write().start_repository();
        let workspace = (self.workspace)();
        let runner = (self.runner)();
        let directory = (self.directory)();
        spawn(async move {
            let response = query(
                &workspace,
                Query {
                    runner,
                    request: RunnerQuery::RepositoryInfo {
                        cwd: directory.into(),
                    },
                },
            )
            .await;
            if !self.requests.write().finish_repository(request_token) {
                return;
            }
            match response {
                Ok(QueryResponse::Success {
                    result: QueryResult::RepositoryInfo(info),
                }) => {
                    if (self.base)().is_empty()
                        && let Some(inferred) = info.inferred_base.clone()
                    {
                        self.base.set(inferred.clone());
                        self.base_input.set(inferred);
                        self.persist();
                    }
                    self.repository.set(Some(info));
                    self.message.set(String::new());
                    self.load_scope(selected_scope, None, DEFAULT_CONTEXT_LINES);
                }
                Ok(response) => {
                    self.repository.set(None);
                    self.message.set(response_message(&response));
                }
                Err(error) => {
                    self.repository.set(None);
                    self.message.set(error);
                }
            }
        });
    }

    fn load_scope(mut self, scope: GitDiffScope, path: Option<String>, context_lines: u32) {
        let request_token = self.requests.write().start_diff(scope);
        let workspace = (self.workspace)();
        let runner = (self.runner)();
        let directory = (self.directory)();
        let base = (self.base)();
        let ignore_whitespace = (self.ignore_whitespace)();
        spawn(async move {
            let response = query(
                &workspace,
                Query {
                    runner,
                    request: RunnerQuery::GitDiff {
                        cwd: directory.into(),
                        scope,
                        base: (!base.is_empty()).then_some(base),
                        ignore_whitespace,
                        path: path.clone(),
                        context_lines,
                    },
                },
            )
            .await;
            if !self.requests.write().finish_diff(scope, request_token) {
                return;
            }
            match response {
                Ok(QueryResponse::Success {
                    result: QueryResult::GitDiff(diff),
                }) => {
                    if let Some(path) = path {
                        let merged = self
                            .caches
                            .peek()
                            .diff(scope)
                            .is_some_and(|existing| merge_expanded_file(existing, &path, diff));
                        if merged {
                            self.message.set(String::new());
                            restore_hash_anchor();
                            return;
                        }
                        self.invalidate_scope(scope);
                        self.load_scope(scope, None, DEFAULT_CONTEXT_LINES);
                        return;
                    }
                    self.caches
                        .write()
                        .store(scope, adapt_git_diff(diff, self.owner));
                    self.message.set(String::new());
                    restore_hash_anchor();
                }
                Ok(response) => self.message.set(response_message(&response)),
                Err(error) => self.message.set(error),
            }
        });
    }

    fn load_file(mut self, path: String) {
        let cwd = self
            .repository
            .read()
            .as_ref()
            .map_or_else(|| (self.directory)(), |repository| repository.root.clone());
        let workspace = (self.workspace)();
        let runner = (self.runner)();
        let request_token = self.requests.write().start_file();
        spawn(async move {
            let response = query(
                &workspace,
                Query {
                    runner,
                    request: RunnerQuery::ReadFile {
                        cwd: cwd.into(),
                        path: path.into(),
                        start_line: 1,
                        line_count: FILE_PREVIEW_LINES,
                    },
                },
            )
            .await;
            if !self.requests.write().finish_file(request_token) {
                return;
            }
            match response {
                Ok(QueryResponse::Success {
                    result: QueryResult::File(file),
                }) => {
                    self.message.set(String::new());
                    self.file_view.set(Some(file));
                }
                Ok(response) => self.message.set(response_message(&response)),
                Err(error) => self.message.set(error),
            }
        });
    }
}

struct FocusListener {
    callback: Closure<dyn FnMut(Event)>,
}

impl FocusListener {
    fn new(focused: Callback<()>) -> Option<Self> {
        let window = web_sys::window()?;
        let callback =
            Closure::wrap(Box::new(move |_event: Event| focused.call(())) as Box<dyn FnMut(Event)>);
        window
            .add_event_listener_with_callback("focus", callback.as_ref().unchecked_ref())
            .ok()?;
        Some(Self { callback })
    }
}

impl Drop for FocusListener {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window.remove_event_listener_with_callback(
                "focus",
                self.callback.as_ref().unchecked_ref(),
            );
        }
    }
}

#[component]
pub(crate) fn ChangesView(
    workspace: String,
    thread: i64,
    controller: ControllerState,
    store: ThreadStore,
) -> Element {
    let key = format!("atra:changes:{workspace}:{thread}");
    let settings_key = use_signal(|| key.clone());
    let workspace_value = use_signal(|| workspace.clone());
    let saved = ChangesSettings::load(&key);
    let mut runners = controller
        .runners()
        .iter()
        .filter(|runner| matches!(runner.lifecycle(), RunnerLifecycle::Running))
        .map(|runner| runner.runner().name.clone())
        .collect::<Vec<_>>();
    runners.sort();
    let fallback_runner = if runners.iter().any(|runner| runner == "host") {
        "host".to_owned()
    } else {
        runners.first().cloned().unwrap_or_default()
    };
    let initial_runner = if runners.iter().any(|runner| runner == &saved.runner) {
        saved.runner.clone()
    } else {
        fallback_runner
    };

    let runner = use_signal(|| initial_runner);
    let mut directory_input = use_signal(|| saved.directory.clone());
    let directory = use_signal(|| saved.directory);
    let scope = use_signal(|| saved.scope);
    let mut base_input = use_signal(|| saved.base.clone());
    let base = use_signal(|| saved.base);
    let ignore_whitespace = use_signal(|| saved.ignore_whitespace);
    let repository = use_signal(|| None::<atra_protocol::RepositoryInfo>);
    let caches = use_signal(DiffCaches::default);
    let requests = use_signal(RequestState::default);
    let message = use_signal(String::new);
    let mut search = use_signal(String::new);
    let mut file_view = use_signal(|| None::<FileContent>);
    let initialized = use_signal(|| false);
    let previous_tools = use_hook(|| Rc::new(RefCell::new(Vec::<String>::new())));
    let previous_running_processes = use_hook(|| Rc::new(RefCell::new(Vec::<String>::new())));
    let preferences = use_context::<DiffPreferences>();
    let mut line_wrap = preferences.line_wrap;
    let state = ChangesState {
        owner: current_scope_id(),
        settings_key,
        workspace: workspace_value,
        runner,
        directory_input,
        directory,
        scope,
        base,
        base_input,
        ignore_whitespace,
        repository,
        caches,
        requests,
        message,
        file_view,
        initialized,
    };
    let loading = requests.read().is_loading();

    use_effect(move || {
        state.initialize();
    });

    use_effect(move || {
        let running_tools = store.read_active_runner_tools().identities(&runner());
        let running_processes = store
            .read_processes()
            .value()
            .unwrap_or_default()
            .iter()
            .filter(|process| {
                process.locator().runner() == runner()
                    && matches!(process.status(), ProcessStatus::Running)
            })
            .map(|process| process.locator().process_id().to_string())
            .collect::<Vec<_>>();
        let tool_completed = removed_item(&previous_tools.borrow(), &running_tools);
        let process_completed =
            removed_item(&previous_running_processes.borrow(), &running_processes);
        *previous_tools.borrow_mut() = running_tools;
        *previous_running_processes.borrow_mut() = running_processes;
        if initialized() && (tool_completed || process_completed) {
            state.external_change();
        }
    });

    let _focus_listener = use_hook(move || {
        let focused = Callback::new(move |()| {
            if !initialized() {
                return;
            }
            state.external_change();
        });
        FocusListener::new(focused).map(Rc::new)
    });

    let current = caches.read().diff(scope()).cloned();
    let current_stale = caches.read().is_stale(scope());
    let selection = current
        .as_ref()
        .map(|diff| diff.select(&search()))
        .unwrap_or_default();

    if let Some(file) = file_view.read().clone() {
        return rsx! {
            section { class: if line_wrap() { "changes-view file-viewer line-wrap" } else { "changes-view file-viewer" },
                header { class: "changes-toolbar",
                    button { onclick: move |_| file_view.set(None), "← Back to diff" }
                    code { "{file.path}" }
                }
                if file.truncated {
                    div { class: "connection-banner", "File is truncated at the viewer limit." }
                }
                div { class: "file-lines",
                    for (index, line) in file.lines.iter().enumerate() {
                        div { class: "file-line",
                            span { class: "file-line-number", "{file.start_line as usize + index}" }
                            code { "{line}" }
                        }
                    }
                }
            }
        };
    }

    rsx! {
        section { class: "changes-view",
            form {
                class: "changes-location",
                onsubmit: move |event| {
                    event.prevent_default();
                    state.apply_directory();
                },
                label {
                    "Runner"
                    select {
                        value: "{runner}",
                        onchange: move |event| state.select_runner(event.value()),
                        if runners.is_empty() {
                            option { value: "", "No running Runners" }
                        }
                        for name in runners {
                            option { value: "{name}", "{name}" }
                        }
                    }
                }
                label { class: "directory-field",
                    "Directory"
                    input {
                        value: "{directory_input}",
                        oninput: move |event| directory_input.set(event.value()),
                    }
                }
                button { r#type: "submit", disabled: loading || runner().is_empty(), "Apply" }
            }
            if let Some(info) = repository.read().clone() {
                div { class: "repository-summary",
                    code { "{info.root}" }
                    span {
                        match info.head {
                            atra_protocol::HeadState::Branch { name, .. } => format!("branch {name}"),
                            atra_protocol::HeadState::Detached { .. } => "detached HEAD".to_owned(),
                            atra_protocol::HeadState::Unborn { .. } => "unborn HEAD".to_owned(),
                        }
                    }
                }
            }
            div { class: "changes-tabs", role: "tablist",
                for tab in [GitDiffScope::Staged, GitDiffScope::Unstaged, GitDiffScope::Base] {
                    button {
                        role: "tab",
                        class: if scope() == tab { "selected" } else { "" },
                        aria_selected: scope() == tab,
                        onclick: move |_| state.select_scope(tab),
                        "{scope_label(tab)}"
                        if let Some(diff) = caches.read().diff(tab) {
                            span { class: "tab-count", "{diff.files.len()}" }
                        }
                    }
                }
            }
            if scope() == GitDiffScope::Base {
                if let Some(info) = repository.read().clone() {
                    form {
                        class: "base-selector",
                        onsubmit: move |event| {
                            event.prevent_default();
                            state.apply_base();
                        },
                        label {
                            "Base"
                            input {
                                list: "base-candidates",
                                value: "{base_input}",
                                oninput: move |event| base_input.set(event.value()),
                            }
                            datalist { id: "base-candidates",
                                for candidate in info.base_candidates {
                                    option { value: "{candidate}" }
                                }
                            }
                        }
                        button { r#type: "submit", disabled: loading, "Apply base" }
                    }
                }
            }
            div { class: "changes-controls",
                input {
                    aria_label: "Filter changed files",
                    placeholder: "Filter paths or status",
                    value: "{search}",
                    oninput: move |event| search.set(event.value()),
                }
                label { class: "checkbox-label",
                    input {
                        r#type: "checkbox",
                        checked: ignore_whitespace(),
                        onchange: move |event| state.set_ignore_whitespace(event.checked()),
                    }
                    "Hide whitespace"
                }
                label { class: "checkbox-label",
                    input {
                        r#type: "checkbox",
                        checked: line_wrap(),
                        onchange: move |event| {
                            line_wrap.set(event.checked());
                            storage_set(
                                crate::DIFF_WRAP_KEY,
                                if event.checked() { "wrap" } else { "scroll" },
                            );
                        },
                    }
                    "Wrap lines"
                }
                button {
                    disabled: loading,
                    onclick: move |_| state.refresh(),
                    "Refresh"
                }
                details { class: "file-index",
                    summary { "Files" }
                    nav {
                            for file in selection.iter() {
                            a {
                                href: "#{file_anchor_id(\"changes\", file.path())}",
                                "{file.path()}"
                            }
                        }
                    }
                }
            }
            if loading {
                p { class: "muted", "Loading changes…" }
            }
            if current_stale {
                div { class: "connection-banner", "This scope is stale. Open or refresh it to update." }
            }
            if !message().is_empty() {
                div { class: "connection-banner", role: "status",
                    span { "{message}" }
                    button {
                        onclick: move |_| state.refresh(),
                        "Retry"
                    }
                }
            }
            if let Some(diff) = current {
                div { class: "changes-stat",
                    span { "{diff.files.len()} files" }
                    span { class: "add", "+{diff.additions}" }
                    span { class: "del", "-{diff.deletions}" }
                    if diff.truncated {
                        strong { "Partial result — diff limit reached" }
                    }
                }
                ReactiveDiffViewer {
                    id_scope: "changes".to_owned(),
                    files: selection,
                    line_wrap: line_wrap(),
                    on_expand: move |path: String| state.load_scope(scope(), Some(path), EXPANDED_CONTEXT_LINES),
                    on_expand_all: move |path: String| {
                        state.load_scope(scope(), Some(path), FULL_CONTEXT_LINES)
                    },
                    on_view_file: move |path: String| state.load_file(path),
                }
            }
        }
    }
}

fn removed_item(previous: &[String], current: &[String]) -> bool {
    previous.iter().any(|item| !current.contains(item))
}

fn merge_expanded_file(existing: &ChangesDiff, path: &str, expanded: GitDiff) -> bool {
    let Some(expanded) = expanded
        .files
        .into_iter()
        .find(|file| file_path(file) == path)
    else {
        return false;
    };
    existing.replace_file(path, adapt_git_file(expanded))
}

fn adapt_git_diff(diff: GitDiff, owner: ScopeId) -> ChangesDiff {
    ChangesDiff {
        files: diff
            .files
            .into_iter()
            .map(adapt_git_file)
            .map(|file| ReactiveDiffFile::new(file, owner))
            .collect(),
        additions: diff.additions,
        deletions: diff.deletions,
        truncated: diff.truncated,
    }
}

fn adapt_git_file(file: atra_protocol::GitDiffFile) -> DiffViewFile {
    DiffViewFile {
        status: match file.change.status() {
            GitFileStatus::Added => DiffViewStatus::Added,
            GitFileStatus::Modified => DiffViewStatus::Modified,
            GitFileStatus::Deleted => DiffViewStatus::Deleted,
            GitFileStatus::Renamed => DiffViewStatus::Renamed,
            GitFileStatus::Copied => DiffViewStatus::Copied,
            GitFileStatus::TypeChanged => DiffViewStatus::TypeChanged,
            GitFileStatus::Unmerged => DiffViewStatus::Unmerged,
        },
        old_path: file.change.old_path().map(str::to_owned),
        new_path: file.change.new_path().map(str::to_owned),
        additions: file.additions,
        deletions: file.deletions,
        mode_change: file
            .mode_change
            .map(|mode| format!("{} → {}", mode.old, mode.new)),
        kind: match file.kind {
            GitFileKind::Text => DiffViewKind::Text,
            GitFileKind::Binary => DiffViewKind::Binary,
            GitFileKind::Symlink { .. } => DiffViewKind::Symlink,
            GitFileKind::Submodule { .. } => DiffViewKind::Submodule,
            GitFileKind::UnsupportedPath => DiffViewKind::UnsupportedPath,
        },
        hunks: file
            .hunks
            .into_iter()
            .map(|hunk| DiffViewHunk {
                header: hunk.header,
                lines: hunk
                    .lines
                    .into_iter()
                    .map(|line| DiffViewLine {
                        kind: match line.kind {
                            atra_protocol::GitDiffLineKind::Context => DiffViewLineKind::Context,
                            atra_protocol::GitDiffLineKind::Addition => DiffViewLineKind::Addition,
                            atra_protocol::GitDiffLineKind::Deletion => DiffViewLineKind::Deletion,
                        },
                        content: line.content,
                        old_line: line.old_line,
                        new_line: line.new_line,
                        no_newline_at_eof: line.no_newline_at_eof,
                    })
                    .collect(),
                truncated: hunk.truncated,
            })
            .collect(),
        truncated: file.truncated,
        message: file.error.as_ref().map(error_message),
    }
}

fn file_path(file: &atra_protocol::GitDiffFile) -> &str {
    file.change.path().unwrap_or("unsupported path")
}

fn scope_label(scope: GitDiffScope) -> &'static str {
    match scope {
        GitDiffScope::Staged => "Staged",
        GitDiffScope::Unstaged => "Unstaged",
        GitDiffScope::Base => "Base",
    }
}

fn error_message(error: &QueryError) -> String {
    match error {
        QueryError::RunnerUnavailable { runner } => format!("Runner {runner} is unavailable."),
        QueryError::NotRepository => "Not a Git repository.".to_owned(),
        QueryError::InvalidRevision { revision } => format!("Invalid revision: {revision}"),
        QueryError::NoMergeBase { revision } => format!("No merge base found for {revision}."),
        QueryError::PathNotFound { path } => format!("Path not found: {path}"),
        QueryError::NotText { path } => format!("{path} is not a UTF-8 text file."),
        QueryError::UnsupportedPath => "The path cannot be represented as UTF-8.".to_owned(),
        QueryError::OutputLimitExceeded => "Query output exceeded the 16 MiB limit.".to_owned(),
        QueryError::TimedOut => "Query exceeded the 30 second limit.".to_owned(),
        QueryError::Internal { message } => message.clone(),
    }
}

fn restore_hash_anchor() {
    spawn(async {
        gloo_timers::future::TimeoutFuture::new(0).await;
        let Some(window) = web_sys::window() else {
            return;
        };
        let Ok(hash) = window.location().hash() else {
            return;
        };
        let Some(id) = hash.strip_prefix('#') else {
            return;
        };
        if let Some(element) = window
            .document()
            .and_then(|document| document.get_element_by_id(id))
        {
            element.scroll_into_view();
        }
    });
}

async fn query(workspace: &str, query: Query) -> Result<QueryResponse, String> {
    let response = Request::post(&format!("/api/workspaces/{workspace}/queries"))
        .header("Content-Type", "application/json")
        .json(&query)
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let succeeded = response.ok();
    let body = response.text().await.map_err(|error| error.to_string())?;
    if succeeded {
        serde_json::from_str(&body)
            .map_err(|error| format!("Invalid query response (HTTP {status}): {error}"))
    } else {
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value["error"].as_str().map(str::to_owned))
            .or_else(|| (!body.trim().is_empty()).then(|| body.trim().to_owned()))
            .unwrap_or_else(|| format!("Query failed with HTTP {status}."));
        Err(message)
    }
}

fn response_message(response: &QueryResponse) -> String {
    match response {
        QueryResponse::Success { .. } => "Runner returned an unexpected query result.".to_owned(),
        QueryResponse::Error { error } => error_message(error),
    }
}
