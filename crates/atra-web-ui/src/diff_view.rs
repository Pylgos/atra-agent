use std::{cell::RefCell, ops::Deref, rc::Rc};

use dioxus::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

use crate::syntax::highlight;

#[derive(Clone, Copy)]
pub(crate) struct DiffPreferences {
    pub line_wrap: Signal<bool>,
}

struct LazyObserver {
    observer: web_sys::IntersectionObserver,
    _callback: Closure<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>,
}

impl LazyObserver {
    fn new(id: &str, mut visible: impl FnMut() + 'static) -> Option<Self> {
        let callback = Closure::wrap(Box::new(
            move |entries: js_sys::Array, observer: web_sys::IntersectionObserver| {
                let intersects = entries.iter().any(|entry| {
                    entry
                        .dyn_into::<web_sys::IntersectionObserverEntry>()
                        .is_ok_and(|entry| entry.is_intersecting())
                });
                if intersects {
                    observer.disconnect();
                    visible();
                }
            },
        )
            as Box<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>);
        let observer =
            web_sys::IntersectionObserver::new(callback.as_ref().unchecked_ref()).ok()?;
        let element = web_sys::window()?.document()?.get_element_by_id(id)?;
        observer.observe(&element);
        Some(Self {
            observer,
            _callback: callback,
        })
    }
}

impl Drop for LazyObserver {
    fn drop(&mut self) {
        self.observer.disconnect();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiffViewStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
    Patch(String),
}

impl DiffViewStatus {
    fn label(&self) -> &str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChanged => "type changed",
            Self::Unmerged => "unmerged",
            Self::Patch(label) => label,
        }
    }

    fn is_deleted(&self) -> bool {
        matches!(self, Self::Deleted)
    }

    fn is_unmerged(&self) -> bool {
        matches!(self, Self::Unmerged)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiffViewKind {
    Text,
    Binary,
    Symlink,
    Submodule,
    UnsupportedPath,
}

impl DiffViewKind {
    fn label(&self) -> &str {
        match self {
            Self::Text => "text",
            Self::Binary => "binary",
            Self::Symlink => "symlink",
            Self::Submodule => "submodule",
            Self::UnsupportedPath => "unsupported path",
        }
    }

    fn is_text(&self) -> bool {
        matches!(self, Self::Text)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffViewFile {
    pub status: DiffViewStatus,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub kind: DiffViewKind,
    pub mode_change: Option<String>,
    pub hunks: Vec<DiffViewHunk>,
    pub truncated: bool,
    pub message: Option<String>,
}

impl DiffViewFile {
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("unsupported path")
    }
}

#[derive(Clone, Debug)]
struct SnapshotFile(Rc<DiffViewFile>);

impl SnapshotFile {
    fn new(file: DiffViewFile) -> Self {
        Self(Rc::new(file))
    }

    fn shared(file: Rc<DiffViewFile>) -> Self {
        Self(file)
    }
}

impl Deref for SnapshotFile {
    type Target = DiffViewFile;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl PartialEq for SnapshotFile {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SnapshotFile {}

#[derive(Clone, Debug, Default)]
struct SnapshotDiffData {
    files: Vec<SnapshotFile>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SnapshotDiff(Rc<SnapshotDiffData>);

impl SnapshotDiff {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.files.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.files.len()
    }

    #[cfg(test)]
    pub(crate) fn file(&self, index: usize) -> Option<&DiffViewFile> {
        self.0.files.get(index).map(Deref::deref)
    }

    pub(crate) fn push(&mut self, file: DiffViewFile) {
        let inner = Rc::make_mut(&mut self.0);
        inner.files.push(SnapshotFile::new(file));
    }
}

impl PartialEq for SnapshotDiff {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SnapshotDiff {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReactiveDiffFile {
    path: Rc<str>,
    status: Rc<str>,
    content: Signal<Rc<DiffViewFile>>,
}

impl ReactiveDiffFile {
    pub(crate) fn new(file: DiffViewFile, owner: ScopeId) -> Self {
        Self {
            path: Rc::from(file.path()),
            status: Rc::from(file.status.label()),
            content: Signal::new_in_scope(Rc::new(file), owner),
        }
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.path.to_lowercase().contains(query)
            || self.status.to_lowercase().contains(query)
    }

    pub(crate) fn replace(&self, file: DiffViewFile) {
        let mut content = self.content;
        content.set(Rc::new(file));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffViewHunk {
    pub header: String,
    pub lines: Vec<DiffViewLine>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffViewLine {
    pub kind: DiffViewLineKind,
    pub content: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub no_newline_at_eof: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffViewLineKind {
    Context,
    Addition,
    Deletion,
}

#[component]
pub(crate) fn SnapshotDiffViewer(
    id_scope: String,
    diff: SnapshotDiff,
    #[props(default)] line_wrap: bool,
) -> Element {
    if diff.is_empty() {
        return rsx! { p { class: "empty-copy", "No changes." } };
    }
    rsx! {
        div { class: "github-diff-list",
            for file in diff.0.files.iter() {
                DiffFile {
                    key: "{file.path()}",
                    id_scope: id_scope.clone(),
                    file: file.clone(),
                    line_wrap,
                }
            }
        }
    }
}

#[component]
pub(crate) fn ReactiveDiffViewer(
    id_scope: String,
    files: Rc<[ReactiveDiffFile]>,
    #[props(default)] line_wrap: bool,
    #[props(default)] on_expand: Option<EventHandler<String>>,
    #[props(default)] on_expand_all: Option<EventHandler<String>>,
    #[props(default)] on_view_file: Option<EventHandler<String>>,
) -> Element {
    if files.is_empty() {
        return rsx! { p { class: "empty-copy", "No changes." } };
    }
    rsx! {
        div { class: "github-diff-list",
            for file in files.iter() {
                ReactiveFile {
                    key: "{file.path()}",
                    id_scope: id_scope.clone(),
                    file: file.clone(),
                    line_wrap,
                    on_expand,
                    on_expand_all,
                    on_view_file,
                }
            }
        }
    }
}

#[component]
fn ReactiveFile(
    id_scope: String,
    file: ReactiveDiffFile,
    line_wrap: bool,
    on_expand: Option<EventHandler<String>>,
    on_expand_all: Option<EventHandler<String>>,
    on_view_file: Option<EventHandler<String>>,
) -> Element {
    let content = file.content.read().clone();
    rsx! {
        DiffFile {
            id_scope,
            file: SnapshotFile::shared(content),
            line_wrap,
            on_expand,
            on_expand_all,
            on_view_file,
        }
    }
}

#[component]
fn DiffFile(
    id_scope: String,
    file: SnapshotFile,
    line_wrap: bool,
    on_expand: Option<EventHandler<String>>,
    on_expand_all: Option<EventHandler<String>>,
    on_view_file: Option<EventHandler<String>>,
) -> Element {
    let mut collapsed = use_signal(|| false);
    let path = file.path().to_owned();
    let id = file_anchor_id(&id_scope, &path);
    let mut body_visible = use_signal(|| false);
    let observer = use_hook(|| Rc::new(RefCell::new(None::<LazyObserver>)));
    use_effect({
        let id = id.clone();
        move || {
            if body_visible() || observer.borrow().is_some() {
                return;
            }
            *observer.borrow_mut() = LazyObserver::new(&id, move || body_visible.set(true));
        }
    });
    let language = language_for_path(&path);
    rsx! {
        article {
            class: if line_wrap { "github-diff-file line-wrap" } else { "github-diff-file" },
            id: "{id}",
            header { class: "github-diff-header",
                button {
                    class: "diff-collapse",
                    aria_expanded: !collapsed(),
                    onclick: move |_| collapsed.toggle(),
                    if collapsed() { "▸" } else { "▾" }
                }
                span { class: "diff-status", "{file.status.label()}" }
                code { class: "diff-path", "{path}" }
                span { class: "diff-stat add", "+{file.additions}" }
                span { class: "diff-stat del", "-{file.deletions}" }
                if !file.kind.is_text() {
                    span { class: "diff-kind", "{file.kind.label()}" }
                }
                if let Some(handler) = on_view_file {
                    if file.new_path.is_some() && !file.status.is_deleted() && file.kind.is_text() {
                        button {
                            class: "diff-action",
                            onclick: {
                                let path = path.clone();
                                move |_| handler.call(path.clone())
                            },
                            "View file"
                        }
                    }
                }
            }
            if !collapsed() && body_visible() {
                if let Some(message) = &file.message {
                    p { class: "diff-message", "{message}" }
                }
                if let Some(mode_change) = &file.mode_change {
                    p { class: "diff-message", "File mode changed: {mode_change}" }
                }
                if file.hunks.is_empty() && file.status.is_unmerged() {
                    p { class: "diff-message", "Resolve this conflict to view its diff." }
                } else if file.hunks.is_empty() && !file.kind.is_text() {
                    p { class: "diff-message", "Content is not shown for this file type." }
                }
                for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                    section { class: "github-diff-hunk",
                        header { class: "diff-hunk-header",
                            code { "{hunk.header}" }
                            if let Some(handler) = on_expand {
                                button {
                                    onclick: {
                                        let path = path.clone();
                                        move |_| handler.call(path.clone())
                                    },
                                    "20 more lines"
                                }
                            }
                            if let Some(handler) = on_expand_all {
                                button {
                                    onclick: {
                                        let path = path.clone();
                                        move |_| handler.call(path.clone())
                                    },
                                    "Expand all"
                                }
                            }
                        }
                        div { class: "diff-lines",
                            for line_index in 0..hunk.lines.len() {
                                DiffLine {
                                    file: file.clone(),
                                    hunk_index,
                                    line_index,
                                    language,
                                }
                            }
                        }
                        if hunk.truncated {
                            div { class: "diff-truncated", "Hunk truncated." }
                        }
                    }
                }
                if file.truncated {
                    div { class: "diff-truncated", "File diff truncated." }
                }
            } else if !collapsed() {
                div { class: "diff-lazy-placeholder", "Loading file diff…" }
            }
        }
    }
}

#[component]
fn DiffLine(
    file: SnapshotFile,
    hunk_index: usize,
    line_index: usize,
    language: &'static str,
) -> Element {
    let line = &file.hunks[hunk_index].lines[line_index];
    let class = match line.kind {
        DiffViewLineKind::Context => "diff-code context",
        DiffViewLineKind::Addition => "diff-code addition",
        DiffViewLineKind::Deletion => "diff-code deletion",
    };
    let old_line = line.old_line;
    let new_line = line.new_line;
    let highlighted = highlight(&line.content, language);
    let no_newline_at_eof = line.no_newline_at_eof;
    rsx! {
        div { class: "{class}",
            button {
                class: "diff-line-number old",
                aria_label: old_line.map(|line| format!("Copy old line {line}")),
                onclick: move |_| {
                    if let Some(number) = old_line {
                        spawn(async move { let _ = crate::copy_text(number.to_string()).await; });
                    }
                },
                {old_line.map(|line| line.to_string()).unwrap_or_default()}
            }
            button {
                class: "diff-line-number new",
                aria_label: new_line.map(|line| format!("Copy new line {line}")),
                onclick: move |_| {
                    if let Some(number) = new_line {
                        spawn(async move { let _ = crate::copy_text(number.to_string()).await; });
                    }
                },
                {new_line.map(|line| line.to_string()).unwrap_or_default()}
            }
            code {
                class: "diff-line-content highlighted",
                dangerous_inner_html: "{highlighted}",
            }
            if no_newline_at_eof {
                span { class: "diff-no-newline", "No newline at end of file" }
            }
        }
    }
}

pub(crate) fn safe_id(path: &str) -> String {
    path.as_bytes()
        .iter()
        .fold(String::with_capacity(path.len() * 2), |mut id, byte| {
            use std::fmt::Write as _;
            write!(id, "{byte:02x}").expect("writing to a String cannot fail");
            id
        })
}

pub(crate) fn file_anchor_id(scope: &str, path: &str) -> String {
    format!("change-file-{}-{}", safe_id(scope), safe_id(path))
}

fn language_for_path(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit_once('.').map(|(_, extension)| extension);
    match extension {
        Some("rs") => "rust",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("ts" | "tsx") => "typescript",
        Some("py") => "python",
        Some("sh" | "bash") => "bash",
        Some("json") => "json",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("md") => "markdown",
        Some("html") => "markup",
        Some("css") => "css",
        Some("sql") => "sql",
        Some("c" | "h") => "c",
        Some("cpp" | "cc" | "hpp") => "cpp",
        Some("java") => "java",
        Some("go") => "go",
        _ => "plain",
    }
}

#[cfg(test)]
mod tests {
    use super::{file_anchor_id, safe_id};

    #[test]
    fn file_anchor_ids_preserve_path_and_viewer_identity() {
        assert_ne!(safe_id("src/foo.rs"), safe_id("src-foo.rs"));
        assert_eq!(safe_id("src/foo.rs"), safe_id("src/foo.rs"));
        assert_ne!(
            file_anchor_id("changes", "src/lib.rs"),
            file_anchor_id("command-1", "src/lib.rs")
        );
    }
}
