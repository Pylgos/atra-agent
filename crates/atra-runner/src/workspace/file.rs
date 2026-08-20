use super::*;

pub(super) struct ScannedUntrackedFile {
    pub(super) lines: u64,
    pub(super) retained_lines: Vec<String>,
    pub(super) terminal_newline: bool,
    pub(super) truncated: bool,
    pub(super) binary: bool,
}

pub(super) async fn scan_untracked_file(
    path: &Path,
    retain_bytes: usize,
) -> Result<ScannedUntrackedFile, QueryError> {
    let mut reader = fs::File::open(path).await.map_err(internal)?;
    let mut chunk = vec![0_u8; 64 * 1024];
    let mut retained = Vec::with_capacity(retain_bytes.min(64 * 1024));
    let mut utf8_tail = Vec::new();
    let mut newlines = 0_u64;
    let mut total_bytes = 0_u64;
    let mut terminal_newline = false;
    let mut binary = false;

    loop {
        let read = reader.read(&mut chunk).await.map_err(internal)?;
        if read == 0 {
            break;
        }
        let bytes = &chunk[..read];
        total_bytes += read as u64;
        newlines += bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
        terminal_newline = bytes.last() == Some(&b'\n');
        binary |= bytes.contains(&0);

        if retained.len() < retain_bytes {
            let keep = (retain_bytes - retained.len()).min(read);
            retained.extend_from_slice(&bytes[..keep]);
        }

        if !binary {
            utf8_tail.extend_from_slice(bytes);
            match std::str::from_utf8(&utf8_tail) {
                Ok(_) => utf8_tail.clear(),
                Err(error) if error.error_len().is_none() => {
                    utf8_tail.drain(..error.valid_up_to());
                    if utf8_tail.len() > 3 {
                        binary = true;
                    }
                }
                Err(_) => binary = true,
            }
        }
    }
    if !utf8_tail.is_empty() {
        binary = true;
    }
    if binary {
        return Ok(ScannedUntrackedFile {
            lines: 0,
            retained_lines: Vec::new(),
            terminal_newline,
            truncated: false,
            binary: true,
        });
    }

    while std::str::from_utf8(&retained).is_err() {
        retained.pop();
    }
    let retained_text = std::str::from_utf8(&retained).expect("retained UTF-8 prefix");
    let lines = newlines + u64::from(total_bytes > 0 && !terminal_newline);
    let retained_lines = retained_text.lines().map(str::to_owned).collect::<Vec<_>>();
    Ok(ScannedUntrackedFile {
        lines,
        truncated: retained.len() as u64 != total_bytes,
        retained_lines,
        terminal_newline,
        binary: false,
    })
}

pub(super) async fn read_file(
    cwd: &Path,
    path: &Path,
    start_line: u32,
    line_count: u32,
) -> Result<FileContent, QueryError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    let metadata = fs::metadata(&path)
        .await
        .map_err(|_| QueryError::PathNotFound {
            path: path.to_string_lossy().into_owned(),
        })?;
    if !metadata.is_file() {
        return Err(QueryError::PathNotFound {
            path: path.to_string_lossy().into_owned(),
        });
    }
    let file = fs::File::open(&path).await.map_err(internal)?;
    let mut lines = BufReader::new(file).lines();
    let start_line = start_line.max(1);
    let requested = usize::try_from(line_count).unwrap_or(usize::MAX);
    let limit = requested.min(MAX_READ_LINES);
    let mut current = 1_u32;
    let mut content = Vec::new();
    let mut bytes = 0_usize;
    let mut truncated = requested > MAX_READ_LINES;
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.as_bytes().contains(&0) {
                    return Err(QueryError::NotText {
                        path: path.to_string_lossy().into_owned(),
                    });
                }
                if current >= start_line {
                    if content.len() >= limit || bytes.saturating_add(line.len()) > MAX_READ_BYTES {
                        truncated = true;
                        break;
                    }
                    bytes += line.len();
                    content.push(line);
                }
                current += 1;
            }
            Ok(None) => {
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(QueryError::NotText {
                    path: path.to_string_lossy().into_owned(),
                });
            }
            Err(error) => return Err(internal(error)),
        }
    }
    Ok(FileContent {
        path: utf8_path(&path)?,
        start_line,
        lines: content,
        truncated,
    })
}
