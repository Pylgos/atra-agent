use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PatchChange {
    Added { path: PathBuf },
    Deleted { path: PathBuf },
    Updated { path: PathBuf },
    Moved { from: PathBuf, to: PathBuf },
}

pub fn apply(patch: &str, cwd: &Path) -> Result<Vec<PatchChange>> {
    let operations = parse(patch)?;
    if operations.is_empty() {
        bail!("No files were modified.");
    }

    let mut affected = Vec::new();
    for operation in operations {
        match operation {
            Operation::Add { path, content } => {
                let resolved = resolve(cwd, &path);
                if let Some(parent) = resolved.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("Failed to create parent directories for {}", path.display())
                    })?;
                }
                fs::write(&resolved, content)
                    .with_context(|| format!("Failed to write file {}", path.display()))?;
                affected.push(PatchChange::Added { path });
            }
            Operation::Delete { path } => {
                let resolved = resolve(cwd, &path);
                fs::remove_file(&resolved)
                    .with_context(|| format!("Failed to delete file {}", path.display()))?;
                affected.push(PatchChange::Deleted { path });
            }
            Operation::Update {
                path,
                move_path,
                chunks,
            } => {
                let resolved = resolve(cwd, &path);
                let original = fs::read_to_string(&resolved)
                    .with_context(|| format!("Failed to read file to update {}", path.display()))?;
                let content = update_content(&original, &path, &chunks)?;
                if let Some(destination) = move_path {
                    let resolved_destination = resolve(cwd, &destination);
                    if let Some(parent) = resolved_destination.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!(
                                "Failed to create parent directories for {}",
                                destination.display()
                            )
                        })?;
                    }
                    fs::write(&resolved_destination, content).with_context(|| {
                        format!("Failed to write file {}", destination.display())
                    })?;
                    fs::remove_file(&resolved)
                        .with_context(|| format!("Failed to remove original {}", path.display()))?;
                    affected.push(PatchChange::Moved {
                        from: path,
                        to: destination,
                    });
                } else {
                    fs::write(&resolved, content)
                        .with_context(|| format!("Failed to write file {}", path.display()))?;
                    affected.push(PatchChange::Updated { path });
                }
            }
        }
    }
    Ok(affected)
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
    let lines: Vec<&str> = patch.trim().lines().collect();
    let lines = unwrap_heredoc(&lines);
    if lines.first().map(|line| line.trim()) != Some(BEGIN) {
        bail!("The first line of the patch must be '{BEGIN}'");
    }
    if lines.last().map(|line| line.trim()) != Some(END) {
        bail!("The last line of the patch must be '{END}'");
    }

    let mut operations = Vec::new();
    let mut index = 1;
    if lines
        .get(index)
        .is_some_and(|line| line.trim().starts_with("*** Environment ID:"))
    {
        let environment = lines[index]
            .trim()
            .strip_prefix("*** Environment ID:")
            .unwrap()
            .trim();
        if environment.is_empty() {
            bail!("apply_patch environment_id cannot be empty");
        }
        index += 1;
    }

    while index < lines.len() - 1 {
        let header = lines[index].trim();
        if let Some(path) = header.strip_prefix(ADD) {
            index += 1;
            let mut content = String::new();
            while index < lines.len() - 1 && !is_operation_header(lines[index]) {
                let line = lines[index]
                    .strip_prefix('+')
                    .with_context(|| format!("invalid add-file line {}", index + 1))?;
                content.push_str(line);
                content.push('\n');
                index += 1;
            }
            if content.is_empty() {
                bail!("Add file hunk for path '{path}' is empty");
            }
            operations.push(Operation::Add {
                path: path.into(),
                content,
            });
        } else if let Some(path) = header.strip_prefix(DELETE) {
            operations.push(Operation::Delete { path: path.into() });
            index += 1;
        } else if let Some(path) = header.strip_prefix(UPDATE) {
            index += 1;
            let mut move_path = None;
            if let Some(destination) = lines
                .get(index)
                .and_then(|line| line.trim_end().strip_prefix(MOVE))
            {
                move_path = Some(destination.into());
                index += 1;
            }
            let mut chunks = Vec::new();
            while index < lines.len() - 1 && !is_operation_header(lines[index]) {
                if lines[index].trim().is_empty()
                    && chunks
                        .last()
                        .is_some_and(|chunk| matches!(chunk, Chunk::Content { eof: true, .. }))
                {
                    index += 1;
                    continue;
                }
                if let Some(start) = lines[index].trim_end().strip_prefix("@ start ") {
                    let start = start
                        .parse::<usize>()
                        .with_context(|| format!("invalid range start on line {}", index + 1))?;
                    index += 1;
                    let first = lines
                        .get(index)
                        .and_then(|line| line.strip_prefix('-'))
                        .with_context(|| {
                            format!(
                                "range start must be followed by a '-' line at {}",
                                index + 1
                            )
                        })?
                        .to_owned();
                    index += 1;
                    let end = if let Some(number) = lines
                        .get(index)
                        .and_then(|line| line.trim_end().strip_prefix("@ end "))
                    {
                        let number = number
                            .parse::<usize>()
                            .with_context(|| format!("invalid range end on line {}", index + 1))?;
                        index += 1;
                        let boundary = lines
                            .get(index)
                            .and_then(|line| line.strip_prefix('-'))
                            .with_context(|| {
                                format!("range end must be followed by a '-' line at {}", index + 1)
                            })?
                            .to_owned();
                        index += 1;
                        Some((number, boundary))
                    } else {
                        None
                    };
                    let mut new = Vec::new();
                    while let Some(line) = lines.get(index).and_then(|line| line.strip_prefix('+'))
                    {
                        new.push(line.to_owned());
                        index += 1;
                    }
                    chunks.push(Chunk::Range {
                        start,
                        first,
                        end,
                        new,
                    });
                    continue;
                }

                let context = match lines[index].trim_end() {
                    "@@" => {
                        index += 1;
                        None
                    }
                    line if line.starts_with("@@ ") => {
                        index += 1;
                        Some(line[3..].to_owned())
                    }
                    _ if chunks.is_empty() => None,
                    line => bail!("expected update hunk header, got '{line}'"),
                };
                let mut old = Vec::new();
                let mut new = Vec::new();
                let mut eof = false;
                while index < lines.len() - 1 {
                    let line = lines[index];
                    if is_operation_header(line)
                        || line.trim_end() == "@@"
                        || line.trim_end().starts_with("@@ ")
                        || line.trim_end().starts_with("@ start ")
                    {
                        break;
                    }
                    if line.trim_end() == "*** End of File" {
                        eof = true;
                        index += 1;
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
                        _ => bail!("invalid update line {}", index + 1),
                    }
                    index += 1;
                }
                if old.is_empty() && new.is_empty() {
                    bail!("Update hunk does not contain any lines");
                }
                chunks.push(Chunk::Content {
                    context,
                    old,
                    new,
                    eof,
                });
            }
            if chunks.is_empty() {
                bail!("Update file hunk for path '{path}' is empty");
            }
            operations.push(Operation::Update {
                path: path.into(),
                move_path,
                chunks,
            });
        } else {
            bail!("'{header}' is not a valid hunk header");
        }
    }
    Ok(operations)
}

fn unwrap_heredoc<'a>(lines: &'a [&'a str]) -> &'a [&'a str] {
    match lines {
        [first, .., last]
            if matches!(*first, "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
                && last.ends_with("EOF")
                && lines.len() >= 4 =>
        {
            &lines[1..lines.len() - 1]
        }
        _ => lines,
    }
}

fn is_operation_header(line: &str) -> bool {
    let line = line.trim();
    line.starts_with(ADD) || line.starts_with(DELETE) || line.starts_with(UPDATE) || line == END
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
