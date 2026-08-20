use super::*;

pub(super) fn parse_numstat(output: &[u8]) -> DiffTotals {
    let mut totals = DiffTotals::default();
    let mut entries = output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        let mut fields = entry.splitn(3, |byte| *byte == b'\t');
        let additions = parse_numstat_count(fields.next());
        let deletions = parse_numstat_count(fields.next());
        let path = match fields.next() {
            Some([]) => {
                let _old_path = entries.next();
                entries.next()
            }
            path => path,
        };
        totals.additions += additions;
        totals.deletions += deletions;
        if let Some(path) = path.and_then(|path| std::str::from_utf8(path).ok()) {
            totals.files.insert(path.to_owned(), (additions, deletions));
        }
    }
    totals
}

pub(super) fn parse_numstat_count(value: Option<&[u8]>) -> u64 {
    value
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

pub(super) fn apply_numstat(diff: &mut GitDiff, totals: &DiffTotals) {
    diff.additions = totals.additions;
    diff.deletions = totals.deletions;
    for file in &mut diff.files {
        if let Some((additions, deletions)) =
            file.change.path().and_then(|path| totals.files.get(path))
        {
            file.additions = *additions;
            file.deletions = *deletions;
        }
    }
}

pub(super) fn added_hunk(
    lines: &[String],
    terminal_newline: bool,
    total_lines: u64,
) -> GitDiffHunk {
    GitDiffHunk {
        header: format!("@@ -0,0 +1,{total_lines} @@"),
        old_start: 0,
        old_lines: 0,
        new_start: 1,
        new_lines: total_lines.min(u32::MAX as u64) as u32,
        lines: lines
            .iter()
            .enumerate()
            .map(|(index, content)| GitDiffLine {
                kind: GitDiffLineKind::Addition,
                content: content.clone(),
                old_line: None,
                new_line: Some(index as u32 + 1),
                no_newline_at_eof: index + 1 == lines.len() && !terminal_newline,
            })
            .collect(),
        truncated: false,
    }
}

pub(super) fn parse_patch_bytes(scope: GitDiffScope, patch: &[u8], truncated: bool) -> GitDiff {
    let patch = if truncated && !patch.ends_with(b"\n") {
        patch
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(&[][..], |end| &patch[..=end])
    } else {
        patch
    };
    let mut result = GitDiff {
        scope,
        files: Vec::new(),
        additions: 0,
        deletions: 0,
        truncated,
    };
    for section in patch_sections(patch) {
        let text = String::from_utf8_lossy(section);
        let mut parsed = parse_patch(scope, &text);
        if std::str::from_utf8(section).is_err() {
            for file in &mut parsed.files {
                file.kind = GitFileKind::Binary;
                file.hunks.clear();
                file.additions = 0;
                file.deletions = 0;
            }
        }
        result.files.append(&mut parsed.files);
    }
    result.additions = result.files.iter().map(|file| file.additions).sum();
    result.deletions = result.files.iter().map(|file| file.deletions).sum();
    if truncated {
        if let Some(file) = result.files.last_mut() {
            file.truncated = true;
        }
    }
    result
}

pub(super) fn patch_sections(patch: &[u8]) -> Vec<&[u8]> {
    const MARKER: &[u8] = b"diff --git ";
    let mut starts = Vec::new();
    if patch.starts_with(MARKER) {
        starts.push(0);
    }
    for index in 1..patch.len() {
        if patch[index - 1] == b'\n' && patch[index..].starts_with(MARKER) {
            starts.push(index);
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(patch.len());
            &patch[*start..end]
        })
        .collect()
}

#[derive(Debug)]
pub(super) struct PendingDiffFile {
    status: GitFileStatus,
    old_path: GitPath,
    new_path: GitPath,
    additions: u64,
    deletions: u64,
    similarity: Option<u8>,
    mode_change: Option<GitModeChange>,
    kind: GitFileKind,
    hunks: Vec<GitDiffHunk>,
}

impl PendingDiffFile {
    fn finish(self) -> GitDiffFile {
        let path = || match (&self.new_path, &self.old_path) {
            (GitPath::Utf8 { .. }, _) => self.new_path.clone(),
            (_, GitPath::Utf8 { .. }) => self.old_path.clone(),
            _ => GitPath::Unsupported,
        };
        let change = match self.status {
            GitFileStatus::Added => GitFileChange::Added {
                path: self.new_path,
            },
            GitFileStatus::Modified => GitFileChange::Modified { path: path() },
            GitFileStatus::Deleted => GitFileChange::Deleted {
                path: self.old_path,
            },
            GitFileStatus::Renamed => GitFileChange::Renamed {
                old_path: self.old_path,
                new_path: self.new_path,
                similarity: self.similarity,
            },
            GitFileStatus::Copied => GitFileChange::Copied {
                old_path: self.old_path,
                new_path: self.new_path,
                similarity: self.similarity,
            },
            GitFileStatus::TypeChanged => GitFileChange::TypeChanged { path: path() },
            GitFileStatus::Unmerged => GitFileChange::Unmerged { path: path() },
        };
        let unsupported = change.path().is_none()
            || matches!(
                &change,
                GitFileChange::Renamed { old_path, .. } | GitFileChange::Copied { old_path, .. }
                    if old_path.as_str().is_none()
            );
        GitDiffFile {
            change,
            additions: self.additions,
            deletions: self.deletions,
            mode_change: self.mode_change,
            kind: if unsupported {
                GitFileKind::UnsupportedPath
            } else {
                self.kind
            },
            hunks: self.hunks,
            truncated: false,
            error: unsupported.then_some(QueryError::UnsupportedPath),
        }
    }
}

pub(super) fn parsed_path(path: Option<String>) -> GitPath {
    path.map_or(GitPath::Unsupported, |value| GitPath::Utf8 { value })
}

pub(super) fn parse_patch(scope: GitDiffScope, patch: &str) -> GitDiff {
    let mut diff = GitDiff {
        scope,
        files: Vec::new(),
        additions: 0,
        deletions: 0,
        truncated: false,
    };
    let mut file: Option<PendingDiffFile> = None;
    let mut hunk: Option<GitDiffHunk> = None;
    let mut old_mode: Option<String> = None;
    let mut old_line = 0_u32;
    let mut new_line = 0_u32;

    let finish_hunk = |file: &mut Option<PendingDiffFile>, hunk: &mut Option<GitDiffHunk>| {
        if let (Some(file), Some(hunk)) = (file.as_mut(), hunk.take()) {
            file.hunks.push(hunk);
        }
    };
    let finish_file =
        |diff: &mut GitDiff, file: &mut Option<PendingDiffFile>, hunk: &mut Option<GitDiffHunk>| {
            finish_hunk(file, hunk);
            if let Some(file) = file.take() {
                let file = file.finish();
                diff.additions += file.additions;
                diff.deletions += file.deletions;
                diff.files.push(file);
            }
        };

    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            finish_file(&mut diff, &mut file, &mut hunk);
            old_mode = None;
            let (old_path, new_path) = parse_diff_paths(line).unwrap_or((None, None));
            file = Some(PendingDiffFile {
                status: GitFileStatus::Modified,
                old_path: parsed_path(old_path),
                new_path: parsed_path(new_path),
                additions: 0,
                deletions: 0,
                similarity: None,
                mode_change: None,
                kind: GitFileKind::Text,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(current) = file.as_mut() else {
            continue;
        };
        if line.starts_with("new file mode ") {
            current.status = GitFileStatus::Added;
            if let Some(mode) = line.strip_prefix("new file mode ") {
                set_kind_from_mode(current, mode);
            }
        } else if line.starts_with("deleted file mode ") {
            current.status = GitFileStatus::Deleted;
            if let Some(mode) = line.strip_prefix("deleted file mode ") {
                set_kind_from_mode(current, mode);
            }
        } else if let Some(mode) = line.strip_prefix("old mode ") {
            old_mode = Some(mode.to_owned());
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            if let Some(old_mode) = old_mode.take() {
                if mode_type(&old_mode) != mode_type(mode) {
                    current.status = GitFileStatus::TypeChanged;
                }
                current.mode_change = Some(GitModeChange {
                    old: old_mode,
                    new: mode.to_owned(),
                });
            }
            set_kind_from_mode(current, mode);
        } else if line.starts_with("index ") && line.ends_with(" 120000") {
            current.kind = GitFileKind::Symlink {
                old_target: None,
                new_target: None,
            };
        } else if let Some(value) = line.strip_prefix("similarity index ") {
            current.similarity = value.trim_end_matches('%').parse().ok();
        } else if let Some(value) = line.strip_prefix("rename from ") {
            current.status = GitFileStatus::Renamed;
            current.old_path = parsed_path(decode_git_path(value));
        } else if let Some(value) = line.strip_prefix("rename to ") {
            current.new_path = parsed_path(decode_git_path(value));
        } else if let Some(value) = line.strip_prefix("copy from ") {
            current.status = GitFileStatus::Copied;
            current.old_path = parsed_path(decode_git_path(value));
        } else if let Some(value) = line.strip_prefix("copy to ") {
            current.new_path = parsed_path(decode_git_path(value));
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            current.kind = GitFileKind::Binary;
        } else if let Some(header) = line.strip_prefix("@@ ") {
            finish_hunk(&mut file, &mut hunk);
            if let Some((old_start_value, old_count, new_start_value, new_count)) =
                parse_hunk_header(header)
            {
                old_line = old_start_value;
                new_line = new_start_value;
                hunk = Some(GitDiffHunk {
                    header: format!("@@ {header}"),
                    old_start: old_start_value,
                    old_lines: old_count,
                    new_start: new_start_value,
                    new_lines: new_count,
                    lines: Vec::new(),
                    truncated: false,
                });
            }
        } else if line == "\\ No newline at end of file" {
            if let Some(last) = hunk.as_mut().and_then(|hunk| hunk.lines.last_mut()) {
                last.no_newline_at_eof = true;
            }
        } else if let Some(active) = hunk.as_mut() {
            let (kind, content, old_number, new_number) = match line.as_bytes().first() {
                Some(b'+') => {
                    let number = new_line;
                    new_line += 1;
                    current.additions += 1;
                    (GitDiffLineKind::Addition, &line[1..], None, Some(number))
                }
                Some(b'-') => {
                    let number = old_line;
                    old_line += 1;
                    current.deletions += 1;
                    (GitDiffLineKind::Deletion, &line[1..], Some(number), None)
                }
                Some(b' ') => {
                    let old_number = old_line;
                    let new_number = new_line;
                    old_line += 1;
                    new_line += 1;
                    (
                        GitDiffLineKind::Context,
                        &line[1..],
                        Some(old_number),
                        Some(new_number),
                    )
                }
                _ => continue,
            };
            active.lines.push(GitDiffLine {
                kind,
                content: content.to_owned(),
                old_line: old_number,
                new_line: new_number,
                no_newline_at_eof: false,
            });
        }
    }
    finish_file(&mut diff, &mut file, &mut hunk);
    classify_special_files(&mut diff);
    diff
}

pub(super) fn mode_type(mode: &str) -> Option<&str> {
    mode.get(..3)
}

pub(super) fn set_kind_from_mode(file: &mut PendingDiffFile, mode: &str) {
    file.kind = match mode_type(mode) {
        Some("120") => GitFileKind::Symlink {
            old_target: None,
            new_target: None,
        },
        Some("160") => GitFileKind::Submodule {
            old_commit: None,
            new_commit: None,
        },
        _ => GitFileKind::Text,
    };
}

pub(super) fn classify_special_files(diff: &mut GitDiff) {
    for file in &mut diff.files {
        let lines = file
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .collect::<Vec<_>>();
        let old_submodule = lines.iter().find_map(|line| {
            (line.kind == GitDiffLineKind::Deletion)
                .then(|| line.content.strip_prefix("Subproject commit "))
                .flatten()
                .map(str::to_owned)
        });
        let new_submodule = lines.iter().find_map(|line| {
            (line.kind == GitDiffLineKind::Addition)
                .then(|| line.content.strip_prefix("Subproject commit "))
                .flatten()
                .map(str::to_owned)
        });
        if old_submodule.is_some() || new_submodule.is_some() {
            file.kind = GitFileKind::Submodule {
                old_commit: old_submodule,
                new_commit: new_submodule,
            };
        } else if matches!(file.kind, GitFileKind::Symlink { .. }) {
            let old_target = lines
                .iter()
                .find(|line| line.kind == GitDiffLineKind::Deletion)
                .map(|line| line.content.clone());
            let new_target = lines
                .iter()
                .find(|line| line.kind == GitDiffLineKind::Addition)
                .map(|line| line.content.clone());
            file.kind = GitFileKind::Symlink {
                old_target,
                new_target,
            };
        } else if file.change.path().is_none() {
            file.kind = GitFileKind::UnsupportedPath;
            file.error = Some(QueryError::UnsupportedPath);
        }
    }
}

pub(super) fn enforce_limits(diff: &mut GitDiff) {
    enforce_limits_to(diff, MAX_DIFF_BYTES, MAX_DIFF_LINES, MAX_DIFF_FILES);
}

pub(super) fn enforce_limits_to(
    diff: &mut GitDiff,
    max_bytes: usize,
    max_lines: usize,
    max_files: usize,
) {
    let mut bytes = 0_usize;
    let mut lines = 0_usize;
    if diff.files.len() > max_files {
        diff.files.truncate(max_files);
        diff.truncated = true;
    }
    for file_index in 0..diff.files.len() {
        for hunk_index in 0..diff.files[file_index].hunks.len() {
            let hunk = &mut diff.files[file_index].hunks[hunk_index];
            let keep = hunk
                .lines
                .iter()
                .take_while(|line| {
                    let fits =
                        bytes.saturating_add(line.content.len()) <= max_bytes && lines < max_lines;
                    if fits {
                        bytes += line.content.len();
                        lines += 1;
                    }
                    fits
                })
                .count();
            if keep < hunk.lines.len() {
                hunk.lines.truncate(keep);
                hunk.truncated = true;
                let file = &mut diff.files[file_index];
                file.hunks.truncate(hunk_index + 1);
                file.truncated = true;
                diff.files.truncate(file_index + 1);
                diff.truncated = true;
                return;
            }
        }
    }
}

pub(super) fn represented_bytes(diff: &GitDiff) -> usize {
    diff.files
        .iter()
        .flat_map(|file| &file.hunks)
        .flat_map(|hunk| &hunk.lines)
        .map(|line| line.content.len())
        .sum()
}

pub(super) fn parse_diff_paths(line: &str) -> Option<(Option<String>, Option<String>)> {
    let value = line.strip_prefix("diff --git ")?;
    let values = split_git_words(value);
    if values.len() != 2 {
        return None;
    }
    Some((
        decode_git_path(values[0]).and_then(strip_diff_prefix),
        decode_git_path(values[1]).and_then(strip_diff_prefix),
    ))
}

pub(super) fn strip_diff_prefix(path: String) -> Option<String> {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .map(str::to_owned)
        .or(Some(path))
}

pub(super) fn split_git_words(value: &str) -> Vec<&str> {
    let bytes = value.as_bytes();
    let mut words = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate() {
        match (*byte, quoted, escaped) {
            (_, true, true) => escaped = false,
            (b'\\', true, false) => escaped = true,
            (b'"', _, false) => quoted = !quoted,
            (b' ', false, false) => {
                if start < index {
                    words.push(&value[start..index]);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < value.len() {
        words.push(&value[start..]);
    }
    words
}

pub(super) fn decode_git_path(value: &str) -> Option<String> {
    let value = value.trim();
    if !(value.starts_with('"') && value.ends_with('"')) {
        return Some(value.to_owned());
    }
    let bytes = value.as_bytes();
    let mut output = Vec::new();
    let mut index = 1;
    while index + 1 < bytes.len() {
        if bytes[index] != b'\\' {
            output.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        if index + 1 >= bytes.len() {
            return None;
        }
        match bytes[index] {
            b'n' => output.push(b'\n'),
            b't' => output.push(b'\t'),
            b'r' => output.push(b'\r'),
            b'\\' => output.push(b'\\'),
            b'"' => output.push(b'"'),
            digit @ b'0'..=b'7' => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    index += 1;
                    let digit = *bytes.get(index)?;
                    if !(b'0'..=b'7').contains(&digit) {
                        return None;
                    }
                    value = value * 8 + u32::from(digit - b'0');
                }
                output.push(u8::try_from(value).ok()?);
            }
            other => output.push(other),
        }
        index += 1;
    }
    String::from_utf8(output).ok()
}

pub(super) fn parse_hunk_header(value: &str) -> Option<(u32, u32, u32, u32)> {
    let end = value.find("@@")?;
    let mut ranges = value[..end].split_whitespace();
    let (old_start, old_count) = parse_range(ranges.next()?, '-')?;
    let (new_start, new_count) = parse_range(ranges.next()?, '+')?;
    Some((old_start, old_count, new_start, new_count))
}

pub(super) fn parse_range(value: &str, prefix: char) -> Option<(u32, u32)> {
    let value = value.strip_prefix(prefix)?;
    let mut parts = value.split(',');
    let start = parts.next()?.parse().ok()?;
    let count = parts.next().map(str::parse).transpose().ok()?.unwrap_or(1);
    Some((start, count))
}
