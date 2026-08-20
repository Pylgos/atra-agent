use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use atra_protocol::{
    FileContent, GitDiff, GitDiffFile, GitDiffHunk, GitDiffLine, GitDiffLineKind, GitDiffScope,
    GitFileChange, GitFileKind, GitFileStatus, GitModeChange, GitPath, HeadState, QueryError,
    QueryResponse, QueryResult, RepositoryInfo, RunnerQuery,
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
    time::timeout,
};

mod file;
mod git;
mod parse;

use file::*;
use git::*;
use parse::*;

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DIFF_BYTES: usize = 16 * 1024 * 1024;
const MAX_GIT_ATOMIC_BYTES: usize = 16 * 1024 * 1024;
const MAX_GIT_STDERR_BYTES: usize = 64 * 1024;
const MAX_DIFF_LINES: usize = 100_000;
const MAX_DIFF_FILES: usize = 5_000;
const MAX_READ_BYTES: usize = 4 * 1024 * 1024;
const MAX_READ_LINES: usize = 20_000;

pub(crate) async fn handle(query: RunnerQuery) -> QueryResponse {
    match timeout(QUERY_TIMEOUT, execute(query)).await {
        Ok(response) => response,
        Err(_) => QueryResponse::Error {
            error: QueryError::TimedOut,
        },
    }
}

async fn execute(query: RunnerQuery) -> QueryResponse {
    let result = match query {
        RunnerQuery::RepositoryInfo { cwd } => {
            repository_info(&cwd).await.map(QueryResult::RepositoryInfo)
        }
        RunnerQuery::GitDiff {
            cwd,
            scope,
            base,
            ignore_whitespace,
            path,
            context_lines,
        } => git_diff(
            &cwd,
            scope,
            base.as_deref(),
            ignore_whitespace,
            path.as_deref(),
            context_lines,
        )
        .await
        .map(QueryResult::GitDiff),
        RunnerQuery::ReadFile {
            cwd,
            path,
            start_line,
            line_count,
        } => read_file(&cwd, &path, start_line, line_count)
            .await
            .map(QueryResult::File),
    };
    match result {
        Ok(result) => QueryResponse::Success { result },
        Err(error) => QueryResponse::Error { error },
    }
}

async fn repository_info(cwd: &Path) -> Result<RepositoryInfo, QueryError> {
    let root = repository_root(cwd).await?;
    let branch = git_optional(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    let commit = git_optional(&root, &["rev-parse", "--verify", "HEAD"]).await?;
    let head = match (branch, commit) {
        (Some(name), Some(commit)) => HeadState::Branch { name, commit },
        (_, Some(commit)) => HeadState::Detached { commit },
        (branch, None) => HeadState::Unborn { branch },
    };

    let refs = git_success(
        &root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )
    .await?;
    let mut base_candidates = refs
        .lines()
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty() && !candidate.ends_with("/HEAD"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    base_candidates.sort();
    base_candidates.dedup();

    let inferred_base = infer_base(&root, &base_candidates).await?;
    Ok(RepositoryInfo {
        root: utf8_path(&root)?,
        head,
        inferred_base,
        base_candidates,
    })
}

async fn infer_base(root: &Path, candidates: &[String]) -> Result<Option<String>, QueryError> {
    if let Some(origin_head) = git_optional(
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await?
    {
        return Ok(Some(origin_head));
    }

    let remote_heads = git_success(
        root,
        &["for-each-ref", "--format=%(refname:short)", "refs/remotes"],
    )
    .await?;
    for remote in remote_heads.lines().filter(|name| name.ends_with("/HEAD")) {
        if let Some(target) = git_optional(
            root,
            &[
                "symbolic-ref",
                "--quiet",
                "--short",
                &format!("refs/remotes/{remote}"),
            ],
        )
        .await?
        {
            return Ok(Some(target));
        }
    }
    for candidate in ["main", "origin/main", "master", "origin/master"] {
        if candidates.iter().any(|value| value == candidate) {
            return Ok(Some(candidate.to_owned()));
        }
    }
    Ok(None)
}

async fn repository_root(cwd: &Path) -> Result<PathBuf, QueryError> {
    if fs::metadata(cwd).await.is_err() {
        return Err(QueryError::PathNotFound {
            path: cwd.to_string_lossy().into_owned(),
        });
    }
    let output = git_output(cwd, &["rev-parse", "--show-toplevel"]).await?;
    if !output.status.success() {
        return Err(QueryError::NotRepository);
    }
    let root = String::from_utf8(output.stdout).map_err(|_| QueryError::UnsupportedPath)?;
    Ok(PathBuf::from(root.trim()))
}

async fn git_diff(
    cwd: &Path,
    scope: GitDiffScope,
    base: Option<&str>,
    ignore_whitespace: bool,
    path: Option<&str>,
    context_lines: u32,
) -> Result<GitDiff, QueryError> {
    let root = repository_root(cwd).await?;
    let scope_args = resolve_diff_scope(&root, scope, base).await?;
    let mut args = vec![
        "-c".to_owned(),
        "color.ui=false".to_owned(),
        "-c".to_owned(),
        "core.quotePath=true".to_owned(),
        "diff".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned(),
        "--full-index".to_owned(),
        "--no-color".to_owned(),
        format!("--unified={context_lines}"),
    ];
    if ignore_whitespace {
        args.push("-w".to_owned());
    }
    args.extend(scope_args.iter().cloned());
    args.push("--".to_owned());
    if let Some(path) = path {
        args.push(path.to_owned());
    }

    let totals = git_numstat(&root, &scope_args, ignore_whitespace, path).await?;
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = git_output_limited(&root, &refs, MAX_DIFF_BYTES).await?;
    if !output.status.success() && !output.truncated {
        return Err(QueryError::Internal {
            message: stderr_message(&output.stderr),
        });
    }
    let mut diff = parse_patch_bytes(scope, &output.stdout, output.truncated);
    apply_numstat(&mut diff, &totals);
    add_unmerged(&root, path, &mut diff).await?;
    if matches!(scope, GitDiffScope::Unstaged | GitDiffScope::Base) {
        add_untracked(&root, path, &mut diff).await?;
    }
    enforce_limits(&mut diff);
    Ok(diff)
}

async fn resolve_diff_scope(
    root: &Path,
    scope: GitDiffScope,
    base: Option<&str>,
) -> Result<Vec<String>, QueryError> {
    match scope {
        GitDiffScope::Staged => Ok(vec!["--cached".to_owned()]),
        GitDiffScope::Unstaged => Ok(Vec::new()),
        GitDiffScope::Base => {
            let base = base.ok_or_else(|| QueryError::InvalidRevision {
                revision: String::new(),
            })?;
            if git_optional(root, &["rev-parse", "--verify", "HEAD"])
                .await?
                .is_none()
            {
                return Err(QueryError::NoMergeBase {
                    revision: base.to_owned(),
                });
            }
            let verified = format!("{base}^{{commit}}");
            if git_optional(root, &["rev-parse", "--verify", &verified])
                .await?
                .is_none()
            {
                return Err(QueryError::InvalidRevision {
                    revision: base.to_owned(),
                });
            }
            let merge_base = git_optional(root, &["merge-base", base, "HEAD"])
                .await?
                .ok_or_else(|| QueryError::NoMergeBase {
                    revision: base.to_owned(),
                })?;
            Ok(vec![merge_base])
        }
    }
}

#[derive(Default)]
struct DiffTotals {
    additions: u64,
    deletions: u64,
    files: HashMap<String, (u64, u64)>,
}

async fn git_numstat(
    root: &Path,
    scope_args: &[String],
    ignore_whitespace: bool,
    path: Option<&str>,
) -> Result<DiffTotals, QueryError> {
    let mut args = vec![
        "-c".to_owned(),
        "color.ui=false".to_owned(),
        "-c".to_owned(),
        "core.quotePath=false".to_owned(),
        "diff".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned(),
        "--numstat".to_owned(),
        "-z".to_owned(),
    ];
    if ignore_whitespace {
        args.push("-w".to_owned());
    }
    args.extend(scope_args.iter().cloned());
    args.push("--".to_owned());
    if let Some(path) = path {
        args.push(path.to_owned());
    }
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = git_output(root, &refs).await?;
    if !output.status.success() {
        return Err(QueryError::Internal {
            message: stderr_message(&output.stderr),
        });
    }
    Ok(parse_numstat(&output.stdout))
}

async fn add_unmerged(
    root: &Path,
    path_filter: Option<&str>,
    diff: &mut GitDiff,
) -> Result<(), QueryError> {
    let mut args = vec!["ls-files", "--unmerged", "-z", "--"];
    if let Some(path) = path_filter {
        args.push(path);
    }
    let output = git_output(root, &args).await?;
    if !output.status.success() {
        return Err(QueryError::Internal {
            message: stderr_message(&output.stderr),
        });
    }
    let paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let separator = entry.iter().position(|byte| *byte == b'\t')?;
            Some(entry[separator + 1..].to_vec())
        })
        .collect::<BTreeSet<_>>();
    for raw_path in paths {
        let Ok(path) = String::from_utf8(raw_path) else {
            diff.files.push(GitDiffFile {
                change: GitFileChange::Unmerged {
                    path: GitPath::Unsupported,
                },
                additions: 0,
                deletions: 0,
                mode_change: None,
                kind: GitFileKind::UnsupportedPath,
                hunks: Vec::new(),
                truncated: false,
                error: Some(QueryError::UnsupportedPath),
            });
            continue;
        };
        if let Some(file) = diff.files.iter_mut().find(|file| {
            file.change.old_path() == Some(&path) || file.change.new_path() == Some(&path)
        }) {
            file.change = GitFileChange::Unmerged {
                path: GitPath::Utf8 {
                    value: path.clone(),
                },
            };
            continue;
        }
        diff.files.push(GitDiffFile {
            change: GitFileChange::Unmerged {
                path: GitPath::Utf8 { value: path },
            },
            additions: 0,
            deletions: 0,
            mode_change: None,
            kind: GitFileKind::Text,
            hunks: Vec::new(),
            truncated: false,
            error: None,
        });
    }
    Ok(())
}

async fn add_untracked(
    root: &Path,
    path_filter: Option<&str>,
    diff: &mut GitDiff,
) -> Result<(), QueryError> {
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
    if let Some(path) = path_filter {
        args.push(path);
    }
    let output = git_output(root, &args).await?;
    if !output.status.success() {
        return Err(QueryError::Internal {
            message: stderr_message(&output.stderr),
        });
    }
    for raw_path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let retain = !diff.truncated && diff.files.len() < MAX_DIFF_FILES;
        let path = match String::from_utf8(raw_path.to_vec()) {
            Ok(path) => path,
            Err(_) => {
                if retain {
                    diff.files.push(GitDiffFile {
                        change: GitFileChange::Added {
                            path: GitPath::Unsupported,
                        },
                        additions: 0,
                        deletions: 0,
                        mode_change: None,
                        kind: GitFileKind::UnsupportedPath,
                        hunks: Vec::new(),
                        truncated: false,
                        error: Some(QueryError::UnsupportedPath),
                    });
                } else {
                    diff.truncated = true;
                }
                continue;
            }
        };
        let full_path = root.join(&path);
        let metadata = match fs::symlink_metadata(&full_path).await {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let mut file = GitDiffFile {
            change: GitFileChange::Added {
                path: GitPath::Utf8 {
                    value: path.clone(),
                },
            },
            additions: 0,
            deletions: 0,
            mode_change: None,
            kind: GitFileKind::Text,
            hunks: Vec::new(),
            truncated: false,
            error: None,
        };
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&full_path)
                .await
                .ok()
                .and_then(|target| target.into_os_string().into_string().ok());
            file.kind = GitFileKind::Symlink {
                old_target: None,
                new_target: target.clone(),
            };
            if let Some(target) = target {
                file.additions = 1;
                if retain {
                    file.hunks.push(added_hunk(&[target], false, 1));
                }
            }
        } else if metadata.is_file() {
            let remaining = if retain {
                MAX_DIFF_BYTES.saturating_sub(represented_bytes(diff))
            } else {
                0
            };
            let scanned = scan_untracked_file(&full_path, remaining).await?;
            file.additions = scanned.lines;
            if scanned.binary {
                file.kind = GitFileKind::Binary;
                file.additions = 0;
            } else if retain {
                file.hunks.push(added_hunk(
                    &scanned.retained_lines,
                    scanned.terminal_newline || scanned.truncated,
                    scanned.lines,
                ));
                file.truncated = scanned.truncated;
                diff.truncated |= scanned.truncated;
            }
        } else {
            continue;
        }
        diff.additions += file.additions;
        if retain {
            diff.files.push(file);
        } else {
            diff.truncated = true;
        }
    }
    Ok(())
}

fn internal(error: impl std::fmt::Display) -> QueryError {
    QueryError::Internal {
        message: error.to_string(),
    }
}

fn utf8_path(path: &Path) -> Result<String, QueryError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(QueryError::UnsupportedPath)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as std_fs;
    use std::process::Command as StdCommand;

    fn git(root: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "user.name", "Atra Test"]);
        git(
            root.path(),
            &["config", "user.email", "atra@example.invalid"],
        );
        root
    }

    #[test]
    fn parses_unified_patch() {
        let diff = parse_patch(
            GitDiffScope::Unstaged,
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n-old\n+new\n same\n",
        );
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.additions, 1);
        assert_eq!(diff.deletions, 1);
        assert_eq!(diff.files[0].change.new_path(), Some("a.txt"));
        assert_eq!(diff.files[0].hunks[0].lines[2].new_line, Some(2));
    }

    #[test]
    fn file_change_paths_match_the_change_shape() {
        let added = parse_patch(
            GitDiffScope::Staged,
            "diff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+new\n",
        );
        let deleted = parse_patch(
            GitDiffScope::Staged,
            "diff --git a/old.txt b/old.txt\ndeleted file mode 100644\n--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
        );

        assert!(matches!(
            &added.files[0].change,
            GitFileChange::Added {
                path: GitPath::Utf8 { value }
            } if value == "new.txt"
        ));
        assert_eq!(added.files[0].change.old_path(), None);
        assert!(matches!(
            &deleted.files[0].change,
            GitFileChange::Deleted {
                path: GitPath::Utf8 { value }
            } if value == "old.txt"
        ));
        assert_eq!(deleted.files[0].change.new_path(), None);
    }

    #[test]
    fn parses_numstat_for_regular_and_renamed_files() {
        let totals = parse_numstat(b"2\t1\tfile.txt\03\t4\t\0old.txt\0new.txt\0");

        assert_eq!(totals.additions, 5);
        assert_eq!(totals.deletions, 5);
        assert_eq!(totals.files["file.txt"], (2, 1));
        assert_eq!(totals.files["new.txt"], (3, 4));
    }

    #[test]
    fn truncated_patch_drops_the_incomplete_line_and_marks_the_last_file() {
        let mut patch =
            b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -0,0 +1 @@\n".to_vec();
        patch.extend_from_slice(&[b'+', 0xc3]);

        let diff = parse_patch_bytes(GitDiffScope::Unstaged, &patch, true);

        assert!(diff.truncated);
        assert_eq!(diff.files.len(), 1);
        assert!(diff.files[0].truncated);
        assert_eq!(diff.files[0].kind, GitFileKind::Text);
        assert!(diff.files[0].hunks[0].lines.is_empty());
    }

    #[test]
    fn decodes_quoted_git_path() {
        assert_eq!(
            decode_git_path("\"a/file\\040name\"").as_deref(),
            Some("a/file name")
        );
    }

    #[test]
    fn distinguishes_permission_and_file_type_mode_changes() {
        let permission = parse_patch(
            GitDiffScope::Unstaged,
            "diff --git a/script b/script\nold mode 100644\nnew mode 100755\n",
        );
        assert_eq!(permission.files[0].change.status(), GitFileStatus::Modified);
        assert_eq!(permission.files[0].kind, GitFileKind::Text);
        assert_eq!(
            permission.files[0].mode_change,
            Some(GitModeChange {
                old: "100644".to_owned(),
                new: "100755".to_owned(),
            })
        );

        let file_type = parse_patch(
            GitDiffScope::Unstaged,
            "diff --git a/link b/link\nold mode 100644\nnew mode 120000\n",
        );
        assert_eq!(
            file_type.files[0].change.status(),
            GitFileStatus::TypeChanged
        );
        assert!(matches!(
            file_type.files[0].kind,
            GitFileKind::Symlink { .. }
        ));
        assert_eq!(
            file_type.files[0].mode_change,
            Some(GitModeChange {
                old: "100644".to_owned(),
                new: "120000".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn does_not_run_textconv_drivers() {
        let root = repository();
        std_fs::write(root.path().join(".gitattributes"), "*.txt diff=atra\n").unwrap();
        std_fs::write(root.path().join("file.txt"), "base\n").unwrap();
        git(root.path(), &["add", ".gitattributes", "file.txt"]);
        git(root.path(), &["commit", "-qm", "base"]);
        git(
            root.path(),
            &[
                "config",
                "diff.atra.textconv",
                "sh -c 'touch textconv-ran; cat \"$1\"' -",
            ],
        );
        std_fs::write(root.path().join("file.txt"), "changed\n").unwrap();

        let diff = git_diff(root.path(), GitDiffScope::Unstaged, None, false, None, 3)
            .await
            .unwrap();

        assert_eq!(diff.files.len(), 1);
        assert!(!root.path().join("textconv-ran").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_quoted_non_utf8_paths_as_unsupported() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = repository();
        let name = OsString::from_vec(b"invalid-\xff.txt".to_vec());
        std_fs::write(root.path().join(&name), "base\n").unwrap();
        let status = StdCommand::new("git")
            .current_dir(root.path())
            .args(["add", "--"])
            .arg(&name)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", root.path())
            .status()
            .unwrap();
        assert!(status.success());
        git(root.path(), &["commit", "-qm", "base"]);
        git(root.path(), &["config", "core.quotePath", "false"]);
        std_fs::write(root.path().join(&name), "changed\n").unwrap();

        let diff = git_diff(root.path(), GitDiffScope::Unstaged, None, false, None, 3)
            .await
            .unwrap();

        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].change.old_path(), None);
        assert_eq!(diff.files[0].change.new_path(), None);
        assert_eq!(diff.files[0].kind, GitFileKind::UnsupportedPath);
        assert_eq!(diff.files[0].error, Some(QueryError::UnsupportedPath));
    }

    #[tokio::test]
    async fn reports_staged_unstaged_and_untracked_changes() {
        let root = repository();
        std_fs::write(root.path().join("tracked.txt"), "base\n").unwrap();
        git(root.path(), &["add", "tracked.txt"]);
        git(root.path(), &["commit", "-qm", "base"]);

        std_fs::write(root.path().join("tracked.txt"), "staged\n").unwrap();
        git(root.path(), &["add", "tracked.txt"]);
        std_fs::write(root.path().join("tracked.txt"), "unstaged\n").unwrap();
        std_fs::write(root.path().join("new.txt"), "new\n").unwrap();

        let staged = git_diff(root.path(), GitDiffScope::Staged, None, false, None, 3)
            .await
            .unwrap();
        assert_eq!(staged.files.len(), 1);
        assert_eq!(staged.files[0].change.new_path(), Some("tracked.txt"));

        let unstaged = git_diff(root.path(), GitDiffScope::Unstaged, None, false, None, 3)
            .await
            .unwrap();
        assert_eq!(unstaged.files.len(), 2);
        assert!(
            unstaged
                .files
                .iter()
                .any(|file| file.change.new_path() == Some("new.txt"))
        );
    }

    #[tokio::test]
    async fn reports_unmerged_files_without_parsing_combined_diffs() {
        let root = repository();
        std_fs::write(root.path().join("file.txt"), "base\n").unwrap();
        git(root.path(), &["add", "file.txt"]);
        git(root.path(), &["commit", "-qm", "base"]);

        git(root.path(), &["checkout", "-qb", "ours"]);
        std_fs::write(root.path().join("file.txt"), "ours\n").unwrap();
        git(root.path(), &["commit", "-qam", "ours"]);

        git(root.path(), &["checkout", "-qb", "theirs", "HEAD~1"]);
        std_fs::write(root.path().join("file.txt"), "theirs\n").unwrap();
        git(root.path(), &["commit", "-qam", "theirs"]);
        git(root.path(), &["checkout", "-q", "ours"]);

        let status = StdCommand::new("git")
            .current_dir(root.path())
            .args(["merge", "theirs"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", root.path())
            .status()
            .unwrap();
        assert!(!status.success());

        let diff = git_diff(root.path(), GitDiffScope::Unstaged, None, false, None, 3)
            .await
            .unwrap();
        let file = diff
            .files
            .iter()
            .find(|file| file.change.new_path() == Some("file.txt"))
            .unwrap();
        assert_eq!(file.change.status(), GitFileStatus::Unmerged);
        assert!(file.hunks.is_empty());
    }

    #[tokio::test]
    async fn honors_disabled_rename_detection() {
        let root = repository();
        std_fs::write(root.path().join("old.txt"), "same\n").unwrap();
        git(root.path(), &["add", "old.txt"]);
        git(root.path(), &["commit", "-qm", "base"]);
        git(root.path(), &["config", "diff.renames", "false"]);
        git(root.path(), &["mv", "old.txt", "new.txt"]);

        let diff = git_diff(root.path(), GitDiffScope::Staged, None, false, None, 3)
            .await
            .unwrap();

        assert!(
            diff.files
                .iter()
                .all(|file| file.change.status() != GitFileStatus::Renamed)
        );
        assert!(
            diff.files
                .iter()
                .any(|file| file.change.status() == GitFileStatus::Deleted)
        );
        assert!(
            diff.files
                .iter()
                .any(|file| file.change.status() == GitFileStatus::Added)
        );
    }

    #[tokio::test]
    async fn marks_untracked_file_without_terminal_newline() {
        let root = repository();
        std_fs::write(root.path().join("new.txt"), "one").unwrap();

        let diff = git_diff(root.path(), GitDiffScope::Unstaged, None, false, None, 3)
            .await
            .unwrap();

        assert!(diff.files[0].hunks[0].lines[0].no_newline_at_eof);
    }

    #[tokio::test]
    async fn treats_invalid_utf8_content_as_binary() {
        let root = repository();
        std_fs::write(root.path().join("file.txt"), "base\n").unwrap();
        git(root.path(), &["add", "file.txt"]);
        git(root.path(), &["commit", "-qm", "base"]);
        std_fs::write(root.path().join("file.txt"), [0xff, b'\n']).unwrap();

        let diff = git_diff(root.path(), GitDiffScope::Unstaged, None, false, None, 3)
            .await
            .unwrap();

        assert_eq!(diff.files[0].kind, GitFileKind::Binary);
        assert!(diff.files[0].hunks.is_empty());
    }

    #[tokio::test]
    async fn counts_all_untracked_lines_without_retaining_all_content() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.txt");
        std_fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();

        let scanned = scan_untracked_file(&path, 5).await.unwrap();

        assert_eq!(scanned.lines, 4);
        assert!(scanned.truncated);
        assert!(
            represented_bytes(&GitDiff {
                scope: GitDiffScope::Unstaged,
                files: vec![GitDiffFile {
                    change: GitFileChange::Added {
                        path: GitPath::Utf8 {
                            value: "large.txt".to_owned(),
                        },
                    },
                    additions: scanned.lines,
                    deletions: 0,
                    mode_change: None,
                    kind: GitFileKind::Text,
                    hunks: vec![added_hunk(
                        &scanned.retained_lines,
                        scanned.terminal_newline,
                        scanned.lines,
                    )],
                    truncated: scanned.truncated,
                    error: None,
                }],
                additions: scanned.lines,
                deletions: 0,
                truncated: scanned.truncated,
            }) <= 5
        );
    }

    #[test]
    fn removes_all_content_after_the_diff_limit() {
        let file = |path: &str, content: &str| GitDiffFile {
            change: GitFileChange::Modified {
                path: GitPath::Utf8 {
                    value: path.to_owned(),
                },
            },
            additions: 1,
            deletions: 0,
            mode_change: None,
            kind: GitFileKind::Text,
            hunks: vec![added_hunk(&[content.to_owned()], true, 1)],
            truncated: false,
            error: None,
        };
        let mut diff = GitDiff {
            scope: GitDiffScope::Unstaged,
            files: vec![file("one", "1234"), file("two", "5678")],
            additions: 2,
            deletions: 0,
            truncated: false,
        };

        enforce_limits_to(&mut diff, 4, 10, 10);

        assert!(diff.truncated);
        assert_eq!(diff.files.len(), 2);
        assert!(diff.files[1].hunks[0].lines.is_empty());
        assert!(diff.files[1].truncated);
        assert_eq!(diff.additions, 2);
        assert_eq!(diff.deletions, 0);
    }

    #[tokio::test]
    async fn supports_unborn_staged_diff_and_reports_base_unavailable() {
        let root = repository();
        std_fs::write(root.path().join("first.txt"), "first\n").unwrap();
        git(root.path(), &["add", "first.txt"]);

        let staged = git_diff(root.path(), GitDiffScope::Staged, None, false, None, 3)
            .await
            .unwrap();
        assert_eq!(staged.files[0].change.status(), GitFileStatus::Added);

        let error = git_diff(
            root.path(),
            GitDiffScope::Base,
            Some("main"),
            false,
            None,
            3,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, QueryError::NoMergeBase { .. }));
    }

    #[tokio::test]
    async fn reads_utf8_lines_with_a_hard_line_limit() {
        let root = tempfile::tempdir().unwrap();
        std_fs::write(root.path().join("file.txt"), "one\ntwo\nthree\n").unwrap();
        let content = read_file(root.path(), Path::new("file.txt"), 2, 1)
            .await
            .unwrap();
        assert_eq!(content.lines, ["two"]);
        assert!(content.truncated);
    }

    #[tokio::test]
    async fn atomic_git_output_fails_instead_of_returning_partial_data() {
        let root = repository();
        let error = git_output_bounded(
            root.path(),
            &["rev-parse", "--show-toplevel"],
            1,
            StdoutLimit::Atomic,
        )
        .await
        .unwrap_err();

        assert_eq!(error, QueryError::OutputLimitExceeded);
    }

    #[tokio::test]
    async fn truncatable_git_output_returns_an_explicit_partial_result() {
        let root = repository();
        let output = git_output_bounded(
            root.path(),
            &["rev-parse", "--show-toplevel"],
            1,
            StdoutLimit::Truncatable,
        )
        .await
        .unwrap();

        assert!(output.truncated);
        assert_eq!(output.stdout.len(), 1);
    }

    #[tokio::test]
    async fn reports_rename_binary_and_symlink_metadata() {
        let root = repository();
        std_fs::write(root.path().join("old.txt"), "rename me\n").unwrap();
        std_fs::write(root.path().join("binary.bin"), [0, 1, 2, 3]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("old.txt", root.path().join("link")).unwrap();
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "-qm", "base"]);

        std_fs::rename(root.path().join("old.txt"), root.path().join("new.txt")).unwrap();
        std_fs::write(root.path().join("binary.bin"), [0, 4, 5, 6]).unwrap();
        #[cfg(unix)]
        {
            std_fs::remove_file(root.path().join("link")).unwrap();
            std::os::unix::fs::symlink("new.txt", root.path().join("link")).unwrap();
        }
        git(root.path(), &["add", "-A"]);

        let diff = git_diff(root.path(), GitDiffScope::Staged, None, false, None, 3)
            .await
            .unwrap();
        assert!(diff.files.iter().any(|file| {
            file.change.status() == GitFileStatus::Renamed
                && file.change.old_path() == Some("old.txt")
                && file.change.new_path() == Some("new.txt")
        }));
        assert!(diff.files.iter().any(|file| {
            file.change.new_path() == Some("binary.bin") && matches!(file.kind, GitFileKind::Binary)
        }));
        #[cfg(unix)]
        assert!(diff.files.iter().any(|file| {
            file.change.new_path() == Some("link")
                && matches!(file.kind, GitFileKind::Symlink { .. })
        }));
    }

    #[tokio::test]
    async fn base_scope_includes_committed_and_worktree_changes() {
        let root = repository();
        std_fs::write(root.path().join("file.txt"), "base\n").unwrap();
        git(root.path(), &["add", "file.txt"]);
        git(root.path(), &["commit", "-qm", "base"]);
        git(root.path(), &["branch", "base"]);

        std_fs::write(root.path().join("file.txt"), "committed\n").unwrap();
        git(root.path(), &["commit", "-qam", "feature"]);
        std_fs::write(root.path().join("file.txt"), "worktree\n").unwrap();
        std_fs::write(root.path().join("untracked.txt"), "new\n").unwrap();

        let diff = git_diff(
            root.path(),
            GitDiffScope::Base,
            Some("base"),
            false,
            None,
            3,
        )
        .await
        .unwrap();
        assert!(
            diff.files
                .iter()
                .any(|file| file.change.new_path() == Some("file.txt"))
        );
        assert!(
            diff.files
                .iter()
                .any(|file| file.change.new_path() == Some("untracked.txt"))
        );
    }
}
