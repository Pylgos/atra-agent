use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";

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

pub fn apply(patch: &str, cwd: &Path) -> ApplyPatchResult {
    let operations = match parse(patch) {
        Ok(operations) if !operations.is_empty() => operations,
        Ok(_) => {
            return ApplyPatchResult::ParseError {
                error: "No files were modified.".to_owned(),
            };
        }
        Err(error) => {
            return ApplyPatchResult::ParseError {
                error: format!("{error:#}"),
            };
        }
    };
    ApplyPatchResult::Operations {
        results: operations
            .into_iter()
            .map(|operation| apply_operation(operation, cwd))
            .collect(),
    }
}

fn apply_operation(operation: Operation, cwd: &Path) -> PatchOperationResult {
    match operation {
        Operation::Add { path, content } => {
            let resolved = resolve(cwd, &path);
            let outcome = apply_add(&resolved, &content)
                .map(|()| file_diff(None, Some(path.clone()), "", &content))
                .into();
            PatchOperationResult::Added { path, outcome }
        }
        Operation::Delete { path } => {
            let resolved = resolve(cwd, &path);
            let diff = fs::read_to_string(&resolved)
                .map(|original| file_diff(Some(path.clone()), None, &original, ""))
                .map_err(|error| format!("Failed to read {} for diff: {error}", path.display()));
            let outcome = match fs::remove_file(&resolved) {
                Ok(()) => PatchOperationOutcome::Applied { diff },
                Err(error) => PatchOperationOutcome::Failed {
                    error: format!("Failed to delete file {}: {error}", path.display()),
                },
            };
            PatchOperationResult::Deleted { path, outcome }
        }
        Operation::Update {
            path,
            move_path,
            chunks,
        } => match move_path {
            Some(destination) => {
                let outcome = apply_move(cwd, &path, &destination, &chunks);
                PatchOperationResult::Moved {
                    from: path,
                    to: destination,
                    outcome,
                }
            }
            None => {
                let outcome = apply_update(cwd, &path, &chunks);
                PatchOperationResult::Updated { path, outcome }
            }
        },
    }
}

impl From<Result<FileDiff>> for PatchOperationOutcome {
    fn from(result: Result<FileDiff>) -> Self {
        match result {
            Ok(diff) => Self::Applied { diff: Ok(diff) },
            Err(error) => Self::Failed {
                error: format!("{error:#}"),
            },
        }
    }
}

fn apply_add(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("add path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create parent directories for {}", path.display()))?;
    atomic_write(path, content, None, false)
        .with_context(|| format!("Failed to write file {}", path.display()))
}

fn apply_update(cwd: &Path, path: &Path, chunks: &[Chunk]) -> PatchOperationOutcome {
    let resolved = resolve(cwd, path);
    let result = (|| {
        let original = fs::read_to_string(&resolved)
            .with_context(|| format!("Failed to read file to update {}", path.display()))?;
        let permissions = fs::metadata(&resolved)
            .with_context(|| format!("Failed to read metadata for {}", path.display()))?
            .permissions();
        let content = update_content(&original, path, chunks)?;
        atomic_write(&resolved, &content, Some(permissions), true)
            .with_context(|| format!("Failed to write file {}", path.display()))?;
        Ok(file_diff(
            Some(path.to_owned()),
            Some(path.to_owned()),
            &original,
            &content,
        ))
    })();
    result.into()
}

fn apply_move(
    cwd: &Path,
    path: &Path,
    destination: &Path,
    chunks: &[Chunk],
) -> PatchOperationOutcome {
    let resolved = resolve(cwd, path);
    let resolved_destination = resolve(cwd, destination);
    let result = (|| {
        let original = fs::read_to_string(&resolved)
            .with_context(|| format!("Failed to read file to update {}", path.display()))?;
        let permissions = fs::metadata(&resolved)
            .with_context(|| format!("Failed to read metadata for {}", path.display()))?
            .permissions();
        let content = if chunks.is_empty() {
            original.clone()
        } else {
            update_content(&original, path, chunks)?
        };
        if resolved == resolved_destination {
            atomic_write(&resolved, &content, Some(permissions), true)
                .with_context(|| format!("Failed to write file {}", path.display()))?;
        } else {
            let parent = resolved_destination
                .parent()
                .context("move destination has no parent directory")?;
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create parent directories for {}",
                    destination.display()
                )
            })?;
            atomic_write(&resolved_destination, &content, Some(permissions), false)
                .with_context(|| format!("Failed to write file {}", destination.display()))?;
            if let Err(error) = fs::remove_file(&resolved) {
                let rollback = fs::remove_file(&resolved_destination);
                let message = match rollback {
                    Ok(()) => format!("Failed to remove original {}: {error}", path.display()),
                    Err(rollback) => format!(
                        "Failed to remove original {}: {error}; failed to remove destination {} during rollback: {rollback}",
                        path.display(),
                        destination.display()
                    ),
                };
                bail!("{message}");
            }
        }
        Ok(file_diff(
            Some(path.to_owned()),
            Some(destination.to_owned()),
            &original,
            &content,
        ))
    })();
    result.into()
}

fn atomic_write(
    path: &Path,
    content: &str,
    permissions: Option<fs::Permissions>,
    overwrite: bool,
) -> Result<()> {
    let parent = path.parent().context("file path has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to create temporary file in {}", parent.display()))?;
    temporary
        .write_all(content.as_bytes())
        .with_context(|| format!("Failed to write temporary file for {}", path.display()))?;
    temporary
        .as_file()
        .set_permissions(permissions.unwrap_or_else(|| fs::Permissions::from_mode(0o644)))
        .with_context(|| format!("Failed to set permissions for {}", path.display()))?;
    if overwrite {
        temporary.persist(path)
    } else {
        temporary.persist_noclobber(path)
    }
    .map_err(|error| error.error)
    .with_context(|| format!("Failed to replace {}", path.display()))?;
    Ok(())
}

fn file_diff(
    old_path: Option<PathBuf>,
    new_path: Option<PathBuf>,
    old: &str,
    new: &str,
) -> FileDiff {
    let diff = TextDiff::from_lines(old, new);
    let hunks = diff
        .grouped_ops(3)
        .into_iter()
        .map(|operations| {
            let old_start = operations.first().unwrap().old_range().start;
            let new_start = operations.first().unwrap().new_range().start;
            let old_end = operations.last().unwrap().old_range().end;
            let new_end = operations.last().unwrap().new_range().end;
            let lines = operations
                .into_iter()
                .flat_map(|operation| diff.iter_changes(&operation))
                .map(|change| DiffLine {
                    kind: match change.tag() {
                        ChangeTag::Equal => DiffLineKind::Context,
                        ChangeTag::Insert => DiffLineKind::Added,
                        ChangeTag::Delete => DiffLineKind::Removed,
                    },
                    old_line: change.old_index().map(|index| index + 1),
                    new_line: change.new_index().map(|index| index + 1),
                    text: change.value().trim_end_matches(['\r', '\n']).to_owned(),
                })
                .collect();
            DiffHunk {
                old_start: if old_end == old_start {
                    old_start
                } else {
                    old_start + 1
                },
                old_count: old_end - old_start,
                new_start: if new_end == new_start {
                    new_start
                } else {
                    new_start + 1
                },
                new_count: new_end - new_start,
                lines,
            }
        })
        .collect();
    FileDiff {
        old_path,
        new_path,
        hunks,
    }
}

fn resolve(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

enum Operation {
    Add {
        path: PathBuf,
        content: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<Chunk>,
    },
}

enum Chunk {
    Content {
        context: Option<String>,
        old: Vec<String>,
        new: Vec<String>,
        eof: bool,
    },
    Range {
        start: usize,
        first: String,
        end: Option<(usize, String)>,
        new: Vec<String>,
    },
}

fn parse(patch: &str) -> Result<Vec<Operation>> {
    Parser {
        lines: patch.lines().collect(),
        index: 0,
    }
    .parse()
}

struct Parser<'a> {
    lines: Vec<&'a str>,
    index: usize,
}

impl Parser<'_> {
    fn parse(mut self) -> Result<Vec<Operation>> {
        let mut operations = Vec::new();
        while self.index < self.lines.len() {
            operations.push(self.parse_operation()?);
        }
        Ok(operations)
    }

    fn parse_operation(&mut self) -> Result<Operation> {
        let header = self.lines[self.index].trim();
        if let Some(path) = header.strip_prefix(ADD) {
            self.index += 1;
            self.parse_add(path)
        } else if let Some(path) = header.strip_prefix(DELETE) {
            self.index += 1;
            Ok(Operation::Delete { path: path.into() })
        } else if let Some(path) = header.strip_prefix(UPDATE) {
            self.index += 1;
            self.parse_update(path)
        } else {
            bail!("'{header}' is not a valid hunk header");
        }
    }

    fn parse_add(&mut self, path: &str) -> Result<Operation> {
        let mut content = String::new();
        while self.index < self.lines.len() && !is_operation_header(self.lines[self.index]) {
            let line = self.lines[self.index]
                .strip_prefix('+')
                .with_context(|| format!("invalid add-file line {}", self.index + 1))?;
            content.push_str(line);
            content.push('\n');
            self.index += 1;
        }
        if content.is_empty() {
            bail!("Add file hunk for path '{path}' is empty");
        }
        Ok(Operation::Add {
            path: path.into(),
            content,
        })
    }

    fn parse_update(&mut self, path: &str) -> Result<Operation> {
        let mut move_path = None;
        if let Some(destination) = self
            .lines
            .get(self.index)
            .and_then(|line| line.trim_end().strip_prefix(MOVE))
        {
            move_path = Some(destination.into());
            self.index += 1;
        }
        let mut chunks = Vec::new();
        while self.index < self.lines.len() && !is_operation_header(self.lines[self.index]) {
            if self.lines[self.index].trim().is_empty()
                && chunks
                    .last()
                    .is_some_and(|chunk| matches!(chunk, Chunk::Content { eof: true, .. }))
            {
                self.index += 1;
                continue;
            }
            if self.lines[self.index].trim_end().starts_with("@ start ") {
                chunks.push(self.parse_range()?);
            } else {
                chunks.push(self.parse_content(chunks.is_empty())?);
            }
        }
        if chunks.is_empty() && move_path.is_none() {
            bail!("Update file hunk for path '{path}' is empty");
        }
        Ok(Operation::Update {
            path: path.into(),
            move_path,
            chunks,
        })
    }

    fn parse_range(&mut self) -> Result<Chunk> {
        let start = self.lines[self.index]
            .trim_end()
            .strip_prefix("@ start ")
            .unwrap()
            .parse::<usize>()
            .with_context(|| format!("invalid range start on line {}", self.index + 1))?;
        self.index += 1;
        let first = self
            .lines
            .get(self.index)
            .and_then(|line| line.strip_prefix('-'))
            .with_context(|| {
                format!(
                    "range start must be followed by a '-' line at {}",
                    self.index + 1
                )
            })?
            .to_owned();
        self.index += 1;
        let end = if let Some(number) = self
            .lines
            .get(self.index)
            .and_then(|line| line.trim_end().strip_prefix("@ end "))
        {
            let number = number
                .parse::<usize>()
                .with_context(|| format!("invalid range end on line {}", self.index + 1))?;
            self.index += 1;
            let boundary = self
                .lines
                .get(self.index)
                .and_then(|line| line.strip_prefix('-'))
                .with_context(|| {
                    format!(
                        "range end must be followed by a '-' line at {}",
                        self.index + 1
                    )
                })?
                .to_owned();
            self.index += 1;
            Some((number, boundary))
        } else {
            None
        };
        let mut new = Vec::new();
        while let Some(line) = self
            .lines
            .get(self.index)
            .and_then(|line| line.strip_prefix('+'))
        {
            new.push(line.to_owned());
            self.index += 1;
        }
        Ok(Chunk::Range {
            start,
            first,
            end,
            new,
        })
    }

    fn parse_content(&mut self, first_chunk: bool) -> Result<Chunk> {
        let context = match self.lines[self.index].trim_end() {
            "@@" => {
                self.index += 1;
                None
            }
            line if line.starts_with("@@ ") => {
                self.index += 1;
                Some(line[3..].to_owned())
            }
            _ if first_chunk => None,
            line => bail!("expected update hunk header, got '{line}'"),
        };
        let mut old = Vec::new();
        let mut new = Vec::new();
        let mut eof = false;
        while self.index < self.lines.len() {
            let line = self.lines[self.index];
            if is_operation_header(line)
                || line.trim_end() == "@@"
                || line.trim_end().starts_with("@@ ")
                || line.trim_end().starts_with("@ start ")
            {
                break;
            }
            if line.trim_end() == "*** End of File" {
                eof = true;
                self.index += 1;
                break;
            }
            match line.as_bytes().first() {
                Some(b' ') => {
                    old.push(line[1..].to_owned());
                    new.push(line[1..].to_owned());
                }
                Some(b'-') => old.push(line[1..].to_owned()),
                Some(b'+') => new.push(line[1..].to_owned()),
                None => {
                    old.push(String::new());
                    new.push(String::new());
                }
                _ => bail!("invalid update line {}", self.index + 1),
            }
            self.index += 1;
        }
        if old.is_empty() && new.is_empty() {
            bail!("Update hunk does not contain any lines");
        }
        Ok(Chunk::Content {
            context,
            old,
            new,
            eof,
        })
    }
}

fn is_operation_header(line: &str) -> bool {
    let line = line.trim();
    line.starts_with(ADD) || line.starts_with(DELETE) || line.starts_with(UPDATE)
}

fn update_content(original: &str, path: &Path, chunks: &[Chunk]) -> Result<String> {
    let mut lines: Vec<String> = original.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let snapshot = lines.clone();
    let mut replacements = Vec::new();
    let mut cursor = 0;

    for chunk in chunks {
        match chunk {
            Chunk::Content {
                context,
                old,
                new,
                eof,
            } => {
                if let Some(context) = context {
                    cursor = seek(&snapshot, std::slice::from_ref(context), cursor, false)
                        .with_context(|| {
                            format!("Failed to find context '{context}' in {}", path.display())
                        })?
                        + 1;
                }
                let mut old = old.as_slice();
                let mut new = new.as_slice();
                let position = if old.is_empty() {
                    snapshot.len()
                } else {
                    let mut position = seek(&snapshot, old, cursor, *eof);
                    if position.is_none() && old.last().is_some_and(String::is_empty) {
                        old = &old[..old.len() - 1];
                        if new.last().is_some_and(String::is_empty) {
                            new = &new[..new.len() - 1];
                        }
                        position = seek(&snapshot, old, cursor, *eof);
                    }
                    position.with_context(|| {
                        format!(
                            "Failed to find expected lines in {}:\n{}",
                            path.display(),
                            old.join("\n")
                        )
                    })?
                };
                replacements.push((position, old.len(), new.to_vec()));
                cursor = position + old.len();
            }
            Chunk::Range {
                start,
                first,
                end,
                new,
            } => {
                if *start == 0 || *start > snapshot.len() {
                    bail!("range start {start} is outside {}", path.display());
                }
                if snapshot[start - 1] != *first {
                    bail!(
                        "range start boundary does not match line {start} in {}",
                        path.display()
                    );
                }
                let end = match end {
                    Some((end, boundary)) => {
                        if *end < *start || *end > snapshot.len() {
                            bail!("range end {end} is outside {}", path.display());
                        }
                        if snapshot[end - 1] != *boundary {
                            bail!(
                                "range end boundary does not match line {end} in {}",
                                path.display()
                            );
                        }
                        *end
                    }
                    None => *start,
                };
                replacements.push((start - 1, end - start + 1, new.clone()));
            }
        }
    }

    replacements.sort_by_key(|replacement| replacement.0);
    for pair in replacements.windows(2) {
        if pair[0].0 + pair[0].1 > pair[1].0 {
            bail!("overlapping update hunks for {}", path.display());
        }
    }
    for (start, length, replacement) in replacements.into_iter().rev() {
        lines.splice(start..start + length, replacement);
    }
    lines.push(String::new());
    Ok(lines.join("\n"))
}

fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let first = if eof {
        lines.len() - pattern.len()
    } else {
        start
    };
    let end = lines.len() - pattern.len();
    for comparison in [
        |left: &str, right: &str| left == right,
        |left: &str, right: &str| left.trim_end() == right.trim_end(),
        |left: &str, right: &str| left.trim() == right.trim(),
    ] {
        for index in first..=end {
            if lines[index..index + pattern.len()]
                .iter()
                .zip(pattern)
                .all(|(left, right)| comparison(left, right))
            {
                return Some(index);
            }
        }
    }
    for index in first..=end {
        if lines[index..index + pattern.len()]
            .iter()
            .zip(pattern)
            .all(|(left, right)| normalize(left) == normalize(right))
        {
            return Some(index);
        }
    }
    None
}

fn normalize(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| match character {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
            '\u{00a0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200a}' | '\u{202f}' | '\u{205f}'
            | '\u{3000}' => ' ',
            character => character,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_without_changes_and_preserves_contents() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("before.txt");
        let destination = directory.path().join("after.txt");
        let contents = b"no trailing newline";
        fs::write(&source, contents).unwrap();

        let result = apply(
            "*** Update File: before.txt\n*** Move to: after.txt",
            directory.path(),
        );

        assert!(matches!(
            result,
            ApplyPatchResult::Operations { ref results }
                if matches!(
                    results.as_slice(),
                    [PatchOperationResult::Moved {
                        from,
                        to,
                        outcome: PatchOperationOutcome::Applied { .. },
                    }] if from == Path::new("before.txt") && to == Path::new("after.txt")
                )
        ));
        assert!(!source.exists());
        assert_eq!(fs::read(destination).unwrap(), contents);
    }

    #[test]
    fn rejects_update_without_changes_or_move() {
        let result = apply("*** Update File: file.txt", Path::new("."));

        assert!(matches!(
            result,
            ApplyPatchResult::ParseError { ref error }
                if error == "Update file hunk for path 'file.txt' is empty"
        ));
    }
}
