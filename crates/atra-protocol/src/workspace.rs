use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub runner: String,
    pub request: RunnerQuery,
}

#[cfg(test)]
mod tests {
    use super::{Query, RunnerQuery};

    #[test]
    fn query_wire_shape_is_strict_and_round_trips() {
        let query = Query {
            runner: "host".to_owned(),
            request: RunnerQuery::RepositoryInfo {
                cwd: "/workspace".into(),
            },
        };
        let value = serde_json::to_value(&query).unwrap();

        assert_eq!(value["runner"], "host");
        assert_eq!(value["request"]["query"], "repository_info");
        assert_eq!(serde_json::from_value::<Query>(value).unwrap(), query);
        assert!(
            serde_json::from_value::<Query>(serde_json::json!({
                "runner": "host",
                "request": {
                    "query": "repository_info",
                    "cwd": "/workspace",
                    "extra": true
                }
            }))
            .is_err()
        );
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "query", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerQuery {
    RepositoryInfo {
        cwd: PathBuf,
    },
    GitDiff {
        cwd: PathBuf,
        scope: GitDiffScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        ignore_whitespace: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        context_lines: u32,
    },
    ReadFile {
        cwd: PathBuf,
        path: PathBuf,
        start_line: u32,
        line_count: u32,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffScope {
    Staged,
    Unstaged,
    Base,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueryResponse {
    Success { result: QueryResult },
    Error { error: QueryError },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueryResult {
    RepositoryInfo(RepositoryInfo),
    GitDiff(GitDiff),
    File(FileContent),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInfo {
    pub root: String,
    pub head: HeadState,
    pub inferred_base: Option<String>,
    pub base_candidates: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum HeadState {
    Branch { name: String, commit: String },
    Detached { commit: String },
    Unborn { branch: Option<String> },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitDiff {
    pub scope: GitDiffScope,
    pub files: Vec<GitDiffFile>,
    pub additions: u64,
    pub deletions: u64,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitDiffFile {
    pub change: GitFileChange,
    pub additions: u64,
    pub deletions: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_change: Option<GitModeChange>,
    pub kind: GitFileKind,
    pub hunks: Vec<GitDiffHunk>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<QueryError>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitModeChange {
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "encoding", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitPath {
    Utf8 { value: String },
    Unsupported,
}

impl GitPath {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Utf8 { value } => Some(value),
            Self::Unsupported => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitFileChange {
    Added {
        path: GitPath,
    },
    Modified {
        path: GitPath,
    },
    Deleted {
        path: GitPath,
    },
    Renamed {
        old_path: GitPath,
        new_path: GitPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        similarity: Option<u8>,
    },
    Copied {
        old_path: GitPath,
        new_path: GitPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        similarity: Option<u8>,
    },
    TypeChanged {
        path: GitPath,
    },
    Unmerged {
        path: GitPath,
    },
}

impl GitFileChange {
    pub fn status(&self) -> GitFileStatus {
        match self {
            Self::Added { .. } => GitFileStatus::Added,
            Self::Modified { .. } => GitFileStatus::Modified,
            Self::Deleted { .. } => GitFileStatus::Deleted,
            Self::Renamed { .. } => GitFileStatus::Renamed,
            Self::Copied { .. } => GitFileStatus::Copied,
            Self::TypeChanged { .. } => GitFileStatus::TypeChanged,
            Self::Unmerged { .. } => GitFileStatus::Unmerged,
        }
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Added { path }
            | Self::Modified { path }
            | Self::Deleted { path }
            | Self::TypeChanged { path }
            | Self::Unmerged { path } => path.as_str(),
            Self::Renamed { new_path, .. } | Self::Copied { new_path, .. } => new_path.as_str(),
        }
    }

    pub fn old_path(&self) -> Option<&str> {
        match self {
            Self::Deleted { path } => path.as_str(),
            Self::Renamed { old_path, .. } | Self::Copied { old_path, .. } => old_path.as_str(),
            Self::Modified { path } | Self::TypeChanged { path } | Self::Unmerged { path } => {
                path.as_str()
            }
            Self::Added { .. } => None,
        }
    }

    pub fn new_path(&self) -> Option<&str> {
        match self {
            Self::Added { path } => path.as_str(),
            Self::Renamed { new_path, .. } | Self::Copied { new_path, .. } => new_path.as_str(),
            Self::Modified { path } | Self::TypeChanged { path } | Self::Unmerged { path } => {
                path.as_str()
            }
            Self::Deleted { .. } => None,
        }
    }

    pub fn is_deleted(&self) -> bool {
        matches!(self, Self::Deleted { .. })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitFileKind {
    Text,
    Binary,
    Symlink {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_target: Option<String>,
    },
    Submodule {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_commit: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_commit: Option<String>,
    },
    UnsupportedPath,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitDiffHunk {
    pub header: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<GitDiffLine>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitDiffLine {
    pub kind: GitDiffLineKind,
    pub content: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub no_newline_at_eof: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffLineKind {
    Context,
    Addition,
    Deletion,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileContent {
    pub path: String,
    pub start_line: u32,
    pub lines: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueryError {
    RunnerUnavailable { runner: String },
    NotRepository,
    InvalidRevision { revision: String },
    NoMergeBase { revision: String },
    PathNotFound { path: String },
    NotText { path: String },
    UnsupportedPath,
    OutputLimitExceeded,
    TimedOut,
    Internal { message: String },
}
