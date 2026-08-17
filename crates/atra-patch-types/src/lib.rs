use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApplyPatchResult {
    ParseError { error: String },
    Operations { results: Vec<PatchOperationResult> },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PatchOperationResult {
    Added {
        path: PathBuf,
        outcome: PatchOperationOutcome,
    },
    Deleted {
        path: PathBuf,
        outcome: PatchOperationOutcome,
    },
    Updated {
        path: PathBuf,
        outcome: PatchOperationOutcome,
    },
    Moved {
        from: PathBuf,
        to: PathBuf,
        outcome: PatchOperationOutcome,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PatchOperationOutcome {
    Applied { diff: Result<FileDiff, String> },
    Failed { error: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct FileDiff {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}
