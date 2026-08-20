use std::collections::HashMap;

use atra_protocol::GitDiffScope;
use serde::{Deserialize, Serialize};
use web_sys::window;

use crate::diff_view::ReactiveDiffFile;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChangesSettings {
    pub runner: String,
    pub directory: String,
    pub scope: GitDiffScope,
    pub base: String,
    pub ignore_whitespace: bool,
}

impl Default for ChangesSettings {
    fn default() -> Self {
        Self {
            runner: String::new(),
            directory: ".".to_owned(),
            scope: GitDiffScope::Unstaged,
            base: String::new(),
            ignore_whitespace: false,
        }
    }
}

impl ChangesSettings {
    pub(crate) fn load(key: &str) -> Self {
        window()
            .and_then(|window| window.local_storage().ok().flatten())
            .and_then(|storage| storage.get_item(key).ok().flatten())
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save(&self, key: &str) {
        let Some(storage) = window().and_then(|window| window.local_storage().ok().flatten())
        else {
            return;
        };
        if let Ok(value) = serde_json::to_string(self) {
            let _ = storage.set_item(key, &value);
        }
    }
}

#[derive(Clone, Default)]
struct ScopeCache {
    diff: Option<ChangesDiff>,
    stale: bool,
}

#[derive(Clone, Default)]
pub(crate) struct ChangesDiff {
    pub(crate) files: Vec<ReactiveDiffFile>,
    pub(crate) additions: u64,
    pub(crate) deletions: u64,
    pub(crate) truncated: bool,
}

impl ChangesDiff {
    pub(crate) fn select(&self, query: &str) -> std::rc::Rc<[ReactiveDiffFile]> {
        let query = query.to_lowercase();
        self.files
            .iter()
            .filter(|file| file.matches(&query))
            .cloned()
            .collect::<Vec<_>>()
            .into()
    }

    pub(crate) fn replace_file(&self, path: &str, file: crate::diff_view::DiffViewFile) -> bool {
        let Some(existing) = self.files.iter().find(|file| file.path() == path) else {
            return false;
        };
        existing.replace(file);
        true
    }
}

#[derive(Clone, Default)]
pub(crate) struct DiffCaches(HashMap<GitDiffScope, ScopeCache>);

impl DiffCaches {
    pub(crate) fn diff(&self, scope: GitDiffScope) -> Option<&ChangesDiff> {
        self.0.get(&scope).and_then(|cache| cache.diff.as_ref())
    }

    pub(crate) fn is_stale(&self, scope: GitDiffScope) -> bool {
        self.0.get(&scope).is_some_and(|cache| cache.stale)
    }

    pub(crate) fn needs_load(&self, scope: GitDiffScope) -> bool {
        self.0
            .get(&scope)
            .is_none_or(|cache| cache.diff.is_none() || cache.stale)
    }

    pub(crate) fn store(&mut self, scope: GitDiffScope, diff: ChangesDiff) {
        self.0.insert(
            scope,
            ScopeCache {
                diff: Some(diff),
                stale: false,
            },
        );
    }

    pub(crate) fn mark_stale(&mut self, scope: GitDiffScope) {
        if let Some(cache) = self.0.get_mut(&scope) {
            cache.stale = true;
        }
    }

    pub(crate) fn mark_all_stale(&mut self) {
        for cache in self.0.values_mut() {
            cache.stale = true;
        }
    }

    pub(crate) fn mark_all_except_stale(&mut self, current: GitDiffScope) {
        for (scope, cache) in &mut self.0 {
            if *scope != current {
                cache.stale = true;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Repository,
    Diff(GitDiffScope),
    File,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestToken {
    kind: RequestKind,
    sequence: u64,
}

#[derive(Clone, Copy, Default)]
struct RequestSlot {
    active: Option<u64>,
}

impl RequestSlot {
    fn start(&mut self, kind: RequestKind, next: &mut u64) -> RequestToken {
        *next = next.wrapping_add(1);
        self.active = Some(*next);
        RequestToken {
            kind,
            sequence: *next,
        }
    }

    fn finish(&mut self, token: RequestToken) -> bool {
        if token.sequence == 0 || self.active != Some(token.sequence) {
            return false;
        }
        self.active = None;
        true
    }

    fn invalidate(&mut self) {
        self.active = None;
    }

    fn is_loading(self) -> bool {
        self.active.is_some()
    }
}

#[derive(Clone, Default)]
pub(crate) struct RequestState {
    next: u64,
    repository: RequestSlot,
    diffs: HashMap<GitDiffScope, RequestSlot>,
    file: RequestSlot,
}

impl RequestState {
    pub(crate) fn start_repository(&mut self) -> RequestToken {
        self.invalidate_all();
        self.repository
            .start(RequestKind::Repository, &mut self.next)
    }

    pub(crate) fn start_diff(&mut self, scope: GitDiffScope) -> RequestToken {
        self.diffs
            .entry(scope)
            .or_default()
            .start(RequestKind::Diff(scope), &mut self.next)
    }

    pub(crate) fn start_file(&mut self) -> RequestToken {
        self.file.start(RequestKind::File, &mut self.next)
    }

    pub(crate) fn finish_repository(&mut self, token: RequestToken) -> bool {
        token.kind == RequestKind::Repository && self.repository.finish(token)
    }

    pub(crate) fn finish_diff(&mut self, scope: GitDiffScope, token: RequestToken) -> bool {
        token.kind == RequestKind::Diff(scope)
            && self
                .diffs
                .get_mut(&scope)
                .is_some_and(|slot| slot.finish(token))
    }

    pub(crate) fn finish_file(&mut self, token: RequestToken) -> bool {
        token.kind == RequestKind::File && self.file.finish(token)
    }

    pub(crate) fn invalidate_all(&mut self) {
        self.repository.invalidate();
        self.file.invalidate();
        self.invalidate_diffs();
    }

    pub(crate) fn invalidate_diff(&mut self, scope: GitDiffScope) {
        if let Some(slot) = self.diffs.get_mut(&scope) {
            slot.invalidate();
        }
    }

    pub(crate) fn invalidate_diffs(&mut self) {
        for slot in self.diffs.values_mut() {
            slot.invalidate();
        }
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.repository.is_loading()
            || self.file.is_loading()
            || self.diffs.values().copied().any(RequestSlot::is_loading)
    }
}

#[cfg(test)]
mod tests {
    use atra_protocol::GitDiffScope;

    use super::RequestState;

    #[test]
    fn changing_diff_options_rejects_every_pending_diff_but_not_file_reads() {
        let mut requests = RequestState::default();
        let unstaged = requests.start_diff(GitDiffScope::Unstaged);
        let staged = requests.start_diff(GitDiffScope::Staged);
        let file = requests.start_file();

        requests.invalidate_diffs();

        assert!(!requests.finish_diff(GitDiffScope::Unstaged, unstaged));
        assert!(!requests.finish_diff(GitDiffScope::Staged, staged));
        assert!(requests.finish_file(file));
        assert!(!requests.is_loading());
    }

    #[test]
    fn a_new_request_supersedes_only_its_own_slot() {
        let mut requests = RequestState::default();
        let old = requests.start_diff(GitDiffScope::Unstaged);
        let new = requests.start_diff(GitDiffScope::Unstaged);

        assert!(!requests.finish_diff(GitDiffScope::Unstaged, old));
        assert!(requests.is_loading());
        assert!(requests.finish_diff(GitDiffScope::Unstaged, new));
        assert!(!requests.is_loading());
    }

    #[test]
    fn repository_selection_invalidates_existing_requests() {
        let mut requests = RequestState::default();
        let diff = requests.start_diff(GitDiffScope::Unstaged);
        let file = requests.start_file();

        let repository = requests.start_repository();

        assert!(!requests.finish_diff(GitDiffScope::Unstaged, diff));
        assert!(!requests.finish_file(file));
        assert!(requests.finish_repository(repository));
        assert!(!requests.is_loading());
    }
}
