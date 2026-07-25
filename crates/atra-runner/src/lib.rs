use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    os::fd::OwnedFd,
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixStream as StdUnixStream,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_protocol::{
    RunnerRequest, RunnerRequestEnvelope, RunnerResponse, RunnerResponseEnvelope, TimeoutAction,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustix::process::{Pid, Signal, kill_process_group};
use sha2::{Digest, Sha256};
use tokio::{
    io::{self, AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, ChildStdin, Command},
    sync::{Mutex, Notify},
    time::{Instant, sleep, sleep_until},
};

mod patch;

pub async fn run_stdio() -> Result<()> {
    serve(BufReader::new(io::stdin()), io::stdout()).await
}

async fn serve(
    mut reader: impl AsyncBufRead + Unpin,
    writer: impl AsyncWrite + Unpin + Send + 'static,
) -> Result<()> {
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .await
        .context("failed to read runner initialize request")?
        == 0
    {
        bail!("controller disconnected before initializing runner");
    }
    let envelope: RunnerRequestEnvelope =
        serde_json::from_str(&line).context("failed to decode runner initialize request")?;
    let writer = Arc::new(Mutex::new(writer));
    let tools = Arc::new(Mutex::new(ToolEnvironment::default()));
    let response = match envelope.request {
        RunnerRequest::Initialize { tools: requested } => {
            let names = requested
                .into_iter()
                .filter(|name| !command_available(name))
                .collect::<Vec<_>>();
            if names.is_empty() {
                tracing::info!("runner initialized");
                RunnerResponse::Ready
            } else {
                RunnerResponse::ToolsRequired { names }
            }
        }
        _ => bail!("runner received a command before initialization"),
    };
    write_response(
        &writer,
        RunnerResponseEnvelope {
            request_id: envelope.request_id,
            response,
        },
    )
    .await?;

    let processes = Arc::new(ProcessManager::default());
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .await
            .context("failed to read runner request")?
            == 0
        {
            return Ok(());
        }
        let envelope: RunnerRequestEnvelope =
            serde_json::from_str(&line).context("failed to decode runner request")?;
        let writer = Arc::clone(&writer);
        let processes = Arc::clone(&processes);
        let tools = Arc::clone(&tools);
        tokio::spawn(async move {
            let response = match handle_request(&processes, &tools, envelope.request).await {
                Ok(response) => response,
                Err(error) => RunnerResponse::Error {
                    message: format!("{error:#}"),
                },
            };
            if let Err(error) = write_response(
                &writer,
                RunnerResponseEnvelope {
                    request_id: envelope.request_id,
                    response,
                },
            )
            .await
            {
                tracing::warn!(error = %format!("{error:#}"), "failed to send runner response");
            }
        });
    }
}

async fn handle_request(
    processes: &ProcessManager,
    tools: &Mutex<ToolEnvironment>,
    request: RunnerRequest,
) -> Result<RunnerResponse> {
    match request {
        RunnerRequest::Initialize { .. } => bail!("runner was initialized more than once"),
        RunnerRequest::InstallTool { name, digest, blob } => {
            let directory = install_tool(&name, &digest, &blob).await?;
            tools.lock().await.add(directory);
            Ok(RunnerResponse::ToolInstalled)
        }
        RunnerRequest::FinishInitialize => {
            tracing::info!("runner initialized with deployed tools");
            Ok(RunnerResponse::Ready)
        }
        RunnerRequest::ExecCommand {
            command,
            cwd,
            background,
            timeout_ms,
            timeout_action,
        } => {
            tracing::debug!(%command, cwd = cwd.as_deref(), "executing command");
            let path = tools.lock().await.path();
            let process = processes.start(command, cwd, path).await?;
            if background {
                return Ok(RunnerResponse::ProcessStarted {
                    process_handle: process.handle,
                });
            }
            processes
                .wait_foreground(
                    process,
                    timeout_ms.map(Duration::from_millis),
                    timeout_action,
                )
                .await
        }
        RunnerRequest::ApplyPatch { patch } => {
            tracing::info!(patch_bytes = patch.len(), "applying patch");
            tracing::trace!(%patch, "patch content");
            let cwd = std::env::current_dir().context("failed to determine runner cwd")?;
            let result = tokio::task::spawn_blocking(move || patch::apply(&patch, &cwd))
                .await
                .context("patch task failed")?;
            Ok(match result {
                Ok(message) => RunnerResponse::PatchResult {
                    success: true,
                    message,
                },
                Err(error) => RunnerResponse::PatchResult {
                    success: false,
                    message: format!("{error:#}"),
                },
            })
        }
        RunnerRequest::WaitProcess {
            process_handle,
            timeout_ms,
        } => {
            processes
                .wait(process_handle, Duration::from_millis(timeout_ms))
                .await
        }
        RunnerRequest::WriteProcess {
            process_handle,
            input,
        } => processes.write(process_handle, input).await,
        RunnerRequest::StopProcess { process_handle } => processes.stop(process_handle).await,
    }
}

async fn write_response(
    writer: &Mutex<impl AsyncWrite + Unpin>,
    response: RunnerResponseEnvelope,
) -> Result<()> {
    let mut response = serde_json::to_vec(&response).context("failed to encode runner response")?;
    response.push(b'\n');
    let mut writer = writer.lock().await;
    writer
        .write_all(&response)
        .await
        .context("failed to write runner response")?;
    writer
        .flush()
        .await
        .context("failed to flush runner stdout")
}

#[derive(Default)]
struct ToolEnvironment {
    directories: Vec<PathBuf>,
}

impl ToolEnvironment {
    fn add(&mut self, directory: PathBuf) {
        if !self.directories.contains(&directory) {
            self.directories.push(directory);
        }
    }

    fn path(&self) -> Option<OsString> {
        if self.directories.is_empty() {
            return None;
        }
        let mut paths = self.directories.clone();
        paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        env::join_paths(paths).ok()
    }
}

fn command_available(name: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| {
            let path = directory.join(name);
            path.is_file()
                && path
                    .metadata()
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        })
    })
}

async fn install_tool(name: &str, digest: &str, blob: &str) -> Result<PathBuf> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid tool name {name:?}");
    }
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid tool digest");
    }

    let compressed = STANDARD
        .decode(blob)
        .context("failed to decode tool blob")?;
    let name = name.to_owned();
    let digest = digest.to_ascii_lowercase();
    tokio::task::spawn_blocking(move || {
        let executable =
            zstd::decode_all(compressed.as_slice()).context("failed to decompress tool blob")?;
        let actual = format!("{:x}", Sha256::digest(&executable));
        if actual != digest {
            bail!("tool {name} digest mismatch: expected {digest}, got {actual}");
        }

        let root = env::var_os("ATRA_TOOL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::temp_dir().join("atra-tools"));
        let directory = root.join(&digest);
        let path = directory.join(&name);
        if !path.exists() {
            std::fs::create_dir_all(&directory).with_context(|| {
                format!("failed to create tool directory {}", directory.display())
            })?;
            let temporary = directory.join(format!(".{name}.tmp-{}", std::process::id()));
            std::fs::write(&temporary, executable)
                .with_context(|| format!("failed to write tool {}", temporary.display()))?;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))
                .with_context(|| {
                    format!("failed to make tool executable {}", temporary.display())
                })?;
            match std::fs::rename(&temporary, &path) {
                Ok(()) => {}
                Err(error) if path.exists() => {
                    let _ = std::fs::remove_file(&temporary);
                    tracing::debug!(%error, tool = name, "tool was installed concurrently");
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to install tool {}", path.display()));
                }
            }
        }
        Ok(directory)
    })
    .await
    .context("tool install task failed")?
}

#[derive(Default)]
struct ProcessManager {
    next_handle: AtomicU64,
    processes: Mutex<HashMap<u64, Arc<ManagedProcess>>>,
}

impl ProcessManager {
    async fn start(
        &self,
        command: String,
        cwd: Option<String>,
        path: Option<OsString>,
    ) -> Result<Arc<ManagedProcess>> {
        let (output_reader, output_writer) =
            StdUnixStream::pair().context("failed to create command output stream")?;
        output_reader
            .set_nonblocking(true)
            .context("failed to configure command output stream")?;
        let stderr_writer = output_writer
            .try_clone()
            .context("failed to clone command output stream")?;

        let mut child = Command::new("bash");
        child
            .args(["-lc", &command])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(OwnedFd::from(output_writer)))
            .stderr(Stdio::from(OwnedFd::from(stderr_writer)))
            .process_group(0)
            .kill_on_drop(true);
        if let Some(path) = path {
            child.env("PATH", path);
        }
        if let Some(cwd) = cwd {
            child.current_dir(cwd);
        }
        let mut child = child
            .spawn()
            .context("failed to execute command with bash")?;
        let stdin = child
            .stdin
            .take()
            .context("command stdin was not available")?;
        let os_pid = Pid::from_raw(
            child
                .id()
                .context("command PID was not available")?
                .try_into()
                .context("command PID is out of range")?,
        )
        .context("command PID was zero")?;
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed) + 1;
        let process = Arc::new(ManagedProcess {
            handle,
            os_pid,
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            output: Mutex::new(Vec::new()),
            output_closed: AtomicBool::new(false),
            changed: Notify::new(),
        });
        self.processes
            .lock()
            .await
            .insert(handle, Arc::clone(&process));

        let output_process = Arc::clone(&process);
        tokio::spawn(async move {
            let mut reader = UnixStream::from_std(output_reader)
                .expect("nonblocking command output stream should be valid");
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(length) => {
                        output_process
                            .output
                            .lock()
                            .await
                            .extend_from_slice(&buffer[..length]);
                        output_process.changed.notify_waiters();
                    }
                    Err(error) => {
                        tracing::warn!(
                            process_handle = output_process.handle,
                            %error,
                            "failed to read command output"
                        );
                        break;
                    }
                }
            }
            output_process.output_closed.store(true, Ordering::Release);
            output_process.changed.notify_waiters();
        });

        tracing::info!(process_handle = handle, "process started");
        Ok(process)
    }

    async fn wait_foreground(
        &self,
        process: Arc<ManagedProcess>,
        timeout: Option<Duration>,
        timeout_action: TimeoutAction,
    ) -> Result<RunnerResponse> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut output = Vec::new();
        loop {
            output.extend(process.take_output().await);
            if let Some(exit_code) = process.finished().await? {
                output.extend(process.take_output().await);
                self.processes.lock().await.remove(&process.handle);
                return Ok(RunnerResponse::ProcessFinished {
                    output: String::from_utf8_lossy(&output).into_owned(),
                    exit_code,
                });
            }

            match deadline {
                Some(deadline) if Instant::now() >= deadline => {
                    return match timeout_action {
                        TimeoutAction::ReturnRunning => Ok(RunnerResponse::ProcessRunning {
                            process_handle: process.handle,
                            output: String::from_utf8_lossy(&output).into_owned(),
                        }),
                        TimeoutAction::Terminate => {
                            let mut stopped = self.stop(process.handle).await?;
                            if let RunnerResponse::ProcessStopped {
                                output: stopped_output,
                            } = &mut stopped
                            {
                                output.extend(stopped_output.as_bytes());
                            }
                            Ok(RunnerResponse::ProcessTimedOut {
                                output: String::from_utf8_lossy(&output).into_owned(),
                            })
                        }
                    };
                }
                Some(deadline) => {
                    tokio::select! {
                        () = process.changed.notified() => {}
                        () = sleep_until(deadline) => {}
                        () = sleep(Duration::from_millis(10)) => {}
                    }
                }
                None => {
                    tokio::select! {
                        () = process.changed.notified() => {}
                        () = sleep(Duration::from_millis(10)) => {}
                    }
                }
            }
        }
    }

    async fn wait(&self, handle: u64, timeout: Duration) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let deadline = Instant::now() + timeout;
        loop {
            let output = process.take_output().await;
            if let Some(exit_code) = process.finished().await? {
                let mut output = output;
                output.extend(process.take_output().await);
                self.processes.lock().await.remove(&handle);
                return Ok(RunnerResponse::ProcessFinished {
                    output: String::from_utf8_lossy(&output).into_owned(),
                    exit_code,
                });
            }
            if !output.is_empty() || Instant::now() >= deadline {
                return Ok(RunnerResponse::ProcessRunning {
                    process_handle: handle,
                    output: String::from_utf8_lossy(&output).into_owned(),
                });
            }
            tokio::select! {
                () = process.changed.notified() => {}
                () = sleep_until(deadline) => {}
                () = sleep(Duration::from_millis(10)) => {}
            }
        }
    }

    async fn write(&self, handle: u64, input: Vec<u8>) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        tracing::info!(
            process_handle = handle,
            input_bytes = input.len(),
            "writing process input"
        );
        tracing::trace!(
            process_handle = handle,
            input = %String::from_utf8_lossy(&input),
            "process input"
        );
        let mut stdin = process.stdin.lock().await;
        stdin
            .as_mut()
            .context("process stdin is closed")?
            .write_all(&input)
            .await
            .context("failed to write process stdin")?;
        Ok(RunnerResponse::InputWritten)
    }

    async fn stop(&self, handle: u64) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let _ = kill_process_group(process.os_pid, Signal::TERM);
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            if process.finished().await?.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = kill_process_group(process.os_pid, Signal::KILL);
            }
            sleep(Duration::from_millis(10)).await;
        }
        while !process.output_closed.load(Ordering::Acquire) {
            tokio::select! {
                () = process.changed.notified() => {}
                () = sleep(Duration::from_millis(10)) => {}
            }
        }
        let output = process.take_output().await;
        self.processes.lock().await.remove(&handle);
        tracing::info!(process_handle = handle, "process stopped");
        Ok(RunnerResponse::ProcessStopped {
            output: String::from_utf8_lossy(&output).into_owned(),
        })
    }

    async fn process(&self, handle: u64) -> Result<Arc<ManagedProcess>> {
        self.processes
            .lock()
            .await
            .get(&handle)
            .cloned()
            .with_context(|| format!("process handle {handle} is not managed by this runner"))
    }
}

struct ManagedProcess {
    handle: u64,
    os_pid: Pid,
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    output: Mutex<Vec<u8>>,
    output_closed: AtomicBool,
    changed: Notify,
}

impl ManagedProcess {
    async fn take_output(&self) -> Vec<u8> {
        std::mem::take(&mut *self.output.lock().await)
    }

    async fn finished(&self) -> Result<Option<Option<i32>>> {
        let status = self
            .child
            .lock()
            .await
            .try_wait()
            .context("failed to inspect command process")?;
        Ok(status
            .filter(|_| self.output_closed.load(Ordering::Acquire))
            .map(|status| status.code()))
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        let _ = kill_process_group(self.os_pid, Signal::KILL);
    }
}
