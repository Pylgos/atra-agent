use super::*;

#[derive(Debug)]
pub(super) struct LimitedOutput {
    pub(super) status: std::process::ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) truncated: bool,
}

#[derive(Clone, Copy)]
pub(super) enum StdoutLimit {
    Atomic,
    Truncatable,
}

pub(super) struct LimitedRead {
    bytes: Vec<u8>,
    exceeded: bool,
}

pub(super) async fn read_limited(
    reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<LimitedRead> {
    let mut bytes = Vec::new();
    reader
        .take(u64::try_from(limit).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)
        .await?;
    let exceeded = bytes.len() > limit;
    bytes.truncate(limit);
    Ok(LimitedRead { bytes, exceeded })
}

pub(super) async fn git_output_bounded(
    cwd: &Path,
    args: &[&str],
    stdout_limit: usize,
    stdout_policy: StdoutLimit,
) -> Result<LimitedOutput, QueryError> {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(internal)?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut stdout_task = tokio::spawn(read_limited(stdout, stdout_limit));
    let mut stderr_task = tokio::spawn(read_limited(stderr, MAX_GIT_STDERR_BYTES));
    let (stdout, stderr) = tokio::select! {
        stdout = &mut stdout_task => {
            let stdout = stdout.map_err(internal)?.map_err(internal)?;
            if stdout.exceeded {
                child.start_kill().map_err(internal)?;
            }
            let stderr = stderr_task.await.map_err(internal)?.map_err(internal)?;
            (stdout, stderr)
        }
        stderr = &mut stderr_task => {
            let stderr = stderr.map_err(internal)?.map_err(internal)?;
            if stderr.exceeded {
                child.start_kill().map_err(internal)?;
            }
            let stdout = stdout_task.await.map_err(internal)?.map_err(internal)?;
            (stdout, stderr)
        }
    };
    if stderr.exceeded || (stdout.exceeded && matches!(stdout_policy, StdoutLimit::Atomic)) {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(QueryError::OutputLimitExceeded);
    }
    if stdout.exceeded {
        let _ = child.start_kill();
    }
    let status = child.wait().await.map_err(internal)?;
    Ok(LimitedOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        truncated: stdout.exceeded,
    })
}

pub(super) async fn git_output_limited(
    cwd: &Path,
    args: &[&str],
    limit: usize,
) -> Result<LimitedOutput, QueryError> {
    git_output_bounded(cwd, args, limit, StdoutLimit::Truncatable).await
}

pub(super) async fn git_optional(cwd: &Path, args: &[&str]) -> Result<Option<String>, QueryError> {
    let output = git_output(cwd, args).await?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout).map_err(|_| QueryError::UnsupportedPath)?;
    Ok(Some(value.trim().to_owned()))
}

pub(super) async fn git_success(cwd: &Path, args: &[&str]) -> Result<String, QueryError> {
    let output = git_output(cwd, args).await?;
    if !output.status.success() {
        return Err(QueryError::Internal {
            message: stderr_message(&output.stderr),
        });
    }
    String::from_utf8(output.stdout).map_err(|_| QueryError::UnsupportedPath)
}

pub(super) async fn git_output(cwd: &Path, args: &[&str]) -> Result<LimitedOutput, QueryError> {
    git_output_bounded(cwd, args, MAX_GIT_ATOMIC_BYTES, StdoutLimit::Atomic).await
}

pub(super) fn stderr_message(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_owned()
}
