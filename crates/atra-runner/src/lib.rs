use std::{
    collections::HashMap,
    env, fs,
    io::ErrorKind,
    os::fd::OwnedFd,
    os::unix::fs::{MetadataExt, PermissionsExt},
    os::unix::net::UnixStream as StdUnixStream,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_patch::apply;
use atra_protocol::{
    CommandEnvironment, CommandOutput, ProcessStatus, RunnerRequest, RunnerRequestEnvelope,
    RunnerResponse, RunnerResponseEnvelope, TimeoutAction,
};
use atra_store::{PreparedTree, Store};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::{
    io::{self, AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
    sync::{Mutex, Notify},
    time::{Instant, sleep, sleep_until},
};

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
    let store = Arc::new(runner_store()?);
    let response = match envelope.request {
        RunnerRequest::Initialize => RunnerResponse::Ready,
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

    let processes = Arc::new(ProcessManager::new()?);
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
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            let response = match handle_request(&processes, &store, envelope.request).await {
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
    store: &Store,
    request: RunnerRequest,
) -> Result<RunnerResponse> {
    match request {
        RunnerRequest::Initialize => bail!("runner was initialized more than once"),
        RunnerRequest::PrepareTree { manifest } => {
            let store = store.clone();
            tokio::task::spawn_blocking(move || match store.prepare_tree(&manifest)? {
                PreparedTree::MissingObjects(digests) => {
                    Ok(RunnerResponse::MissingObjects { digests })
                }
                PreparedTree::Ready { digest, path } => Ok(RunnerResponse::TreeReady {
                    digest,
                    path: path
                        .into_os_string()
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("tree path is not valid UTF-8"))?,
                }),
            })
            .await
            .context("tree preparation task failed")?
        }
        RunnerRequest::UploadObject {
            digest,
            executable,
            blob,
        } => {
            let store = store.clone();
            tokio::task::spawn_blocking(move || {
                let compressed = STANDARD
                    .decode(blob)
                    .context("failed to decode object blob")?;
                let decoder = zstd::Decoder::new(compressed.as_slice())
                    .context("failed to decompress object blob")?;
                store.put_object(&digest, executable, decoder)?;
                Ok(RunnerResponse::ObjectStored)
            })
            .await
            .context("object upload task failed")?
        }
        RunnerRequest::ExecCommand {
            command,
            background,
            timeout_ms,
            timeout_action,
            environment,
        } => {
            tracing::debug!(%command, "executing command");
            let process = processes.start(command, environment).await?;
            if background {
                return Ok(RunnerResponse::ProcessStarted {
                    process_handle: process.handle.clone(),
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
        RunnerRequest::StartCommand {
            command,
            environment,
        } => {
            tracing::debug!(%command, "starting command");
            let process = processes.start(command, environment).await?;
            Ok(RunnerResponse::ProcessStarted {
                process_handle: process.handle.clone(),
            })
        }
        RunnerRequest::ApplyPatch { patch } => {
            tracing::info!(patch_bytes = patch.len(), "applying patch");
            tracing::trace!(%patch, "patch content");
            let cwd = std::env::current_dir().context("failed to determine runner cwd")?;
            let result = tokio::task::spawn_blocking(move || apply(&patch, &cwd))
                .await
                .context("patch task failed")?;
            Ok(RunnerResponse::PatchCompleted { result })
        }
        RunnerRequest::WaitProcess {
            process_handle,
            timeout_ms,
        } => {
            processes
                .wait(&process_handle, Duration::from_millis(timeout_ms))
                .await
        }
        RunnerRequest::StopProcess { process_handle } => processes.stop(&process_handle).await,
        RunnerRequest::InspectProcess { process_handle } => {
            processes.inspect(&process_handle).await
        }
        RunnerRequest::ProcessStatus { process_handle } => processes.status(&process_handle).await,
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

fn runner_store() -> Result<Store> {
    let root = env::temp_dir().join(format!("atra-{}", rustix::process::geteuid().as_raw()));
    match fs::create_dir(&root) {
        Ok(()) => fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure store {}", root.display()))?,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&root)
                .with_context(|| format!("failed to inspect store {}", root.display()))?;
            if !metadata.is_dir()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.permissions().mode() & 0o077 != 0
            {
                bail!("unsafe Runner store {}", root.display());
            }
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to create store {}", root.display()));
        }
    }
    Store::open(root)
}

struct ProcessManager {
    processes: Mutex<HashMap<String, Arc<ManagedProcess>>>,
    output_directory: tempfile::TempDir,
}

impl ProcessManager {
    fn new() -> Result<Self> {
        Ok(Self {
            processes: Mutex::new(HashMap::new()),
            output_directory: tempfile::tempdir()
                .context("failed to create command output directory")?,
        })
    }

    async fn start(
        &self,
        command: String,
        environment: CommandEnvironment,
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
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(output_writer)))
            .stderr(Stdio::from(OwnedFd::from(stderr_writer)))
            .process_group(0)
            .kill_on_drop(true);
        child.envs(&environment.set);
        if !environment.prepend_path.is_empty() || !environment.append_path.is_empty() {
            let base = environment
                .set
                .get("PATH")
                .map(Into::into)
                .or_else(|| env::var_os("PATH"))
                .unwrap_or_default();
            let mut paths = environment
                .prepend_path
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            paths.extend(env::split_paths(&base));
            paths.extend(environment.append_path.iter().map(PathBuf::from));
            child.env(
                "PATH",
                env::join_paths(paths).context("command PATH contains an invalid path")?,
            );
        }
        let child = child
            .spawn()
            .context("failed to execute command with bash")?;
        let os_pid = Pid::from_raw(
            child
                .id()
                .context("command PID was not available")?
                .try_into()
                .context("command PID is out of range")?,
        )
        .context("command PID was zero")?;
        let mut processes = self.processes.lock().await;
        let handle = loop {
            let handle = atra_id::generate();
            if !processes.contains_key(&handle) {
                break handle;
            }
        };
        let full_output_path = self.output_directory.path().join(&handle);
        let full_output = std::fs::File::create(&full_output_path)
            .context("failed to create full command output file")?;
        let process = Arc::new(ManagedProcess {
            handle: handle.clone(),
            os_pid,
            child: Mutex::new(child),
            output: Mutex::new(OutputBuffer::default()),
            output_tail: Mutex::new(TailBuffer::default()),
            full_output_path,
            output_closed: AtomicBool::new(false),
            changed: Notify::new(),
        });
        processes.insert(handle.clone(), Arc::clone(&process));
        drop(processes);

        let output_process = Arc::clone(&process);
        tokio::spawn(async move {
            let mut full_output = tokio::fs::File::from_std(full_output);
            let mut reader = UnixStream::from_std(output_reader)
                .expect("nonblocking command output stream should be valid");
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(length) => {
                        if let Err(error) = full_output.write_all(&buffer[..length]).await {
                            tracing::warn!(
                                process_handle = %output_process.handle,
                                %error,
                                "failed to save full command output"
                            );
                            break;
                        }
                        output_process.output.lock().await.append(&buffer[..length]);
                        output_process
                            .output_tail
                            .lock()
                            .await
                            .append(&buffer[..length]);
                        output_process.changed.notify_waiters();
                    }
                    Err(error) => {
                        tracing::warn!(
                            process_handle = %output_process.handle,
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

        tracing::info!(process_handle = %handle, "process started");
        Ok(process)
    }

    async fn wait_foreground(
        &self,
        process: Arc<ManagedProcess>,
        timeout: Option<Duration>,
        timeout_action: TimeoutAction,
    ) -> Result<RunnerResponse> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if let Some(exit_code) = process.finished().await? {
                let output = process.take_output().await;
                self.processes.lock().await.remove(&process.handle);
                return Ok(RunnerResponse::ProcessFinished {
                    output: output.finish(process.full_output_path.clone()),
                    exit_code,
                });
            }

            match deadline {
                Some(deadline) if Instant::now() >= deadline => {
                    return match timeout_action {
                        TimeoutAction::ReturnRunning => Ok(RunnerResponse::ProcessRunning {
                            process_handle: process.handle.clone(),
                            output: process
                                .take_output()
                                .await
                                .finish(process.full_output_path.clone()),
                        }),
                        TimeoutAction::Terminate => match self.stop(&process.handle).await? {
                            RunnerResponse::ProcessStopped { output } => {
                                Ok(RunnerResponse::ProcessTimedOut { output })
                            }
                            _ => unreachable!("stop always returns ProcessStopped"),
                        },
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

    async fn wait(&self, handle: &str, timeout: Duration) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let deadline = Instant::now() + timeout;
        loop {
            let output = process.take_output().await;
            if let Some(exit_code) = process.finished().await? {
                let mut output = output;
                output.append_output(process.take_output().await);
                self.processes.lock().await.remove(handle);
                return Ok(RunnerResponse::ProcessFinished {
                    output: output.finish(process.full_output_path.clone()),
                    exit_code,
                });
            }
            if !output.bytes.is_empty() || output.omitted_bytes != 0 || Instant::now() >= deadline {
                return Ok(RunnerResponse::ProcessRunning {
                    process_handle: handle.to_owned(),
                    output: output.finish(process.full_output_path.clone()),
                });
            }
            tokio::select! {
                () = process.changed.notified() => {}
                () = sleep_until(deadline) => {}
                () = sleep(Duration::from_millis(10)) => {}
            }
        }
    }

    async fn stop(&self, handle: &str) -> Result<RunnerResponse> {
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
        self.processes.lock().await.remove(handle);
        tracing::info!(process_handle = %handle, "process stopped");
        Ok(RunnerResponse::ProcessStopped {
            output: output.finish(process.full_output_path.clone()),
        })
    }

    async fn inspect(&self, handle: &str) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let process_status = match process.finished().await? {
            Some(exit_code) => ProcessStatus::Exited { exit_code },
            None => ProcessStatus::Running,
        };
        let (output_tail, omitted_bytes) = process.output_tail.lock().await.snapshot();
        Ok(RunnerResponse::ProcessInspected {
            process_status,
            output_tail,
            omitted_bytes,
        })
    }

    async fn status(&self, handle: &str) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let process_status = match process.finished().await? {
            Some(exit_code) => ProcessStatus::Exited { exit_code },
            None => ProcessStatus::Running,
        };
        Ok(RunnerResponse::ProcessStatus { process_status })
    }

    async fn process(&self, handle: &str) -> Result<Arc<ManagedProcess>> {
        self.processes
            .lock()
            .await
            .get(handle)
            .cloned()
            .with_context(|| format!("process handle {handle} is not managed by this runner"))
    }
}

struct ManagedProcess {
    handle: String,
    os_pid: Pid,
    child: Mutex<Child>,
    output: Mutex<OutputBuffer>,
    output_tail: Mutex<TailBuffer>,
    full_output_path: PathBuf,
    output_closed: AtomicBool,
    changed: Notify,
}

impl ManagedProcess {
    async fn take_output(&self) -> OutputBuffer {
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

const MAX_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_TAIL_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct OutputBuffer {
    bytes: Vec<u8>,
    omitted_bytes: usize,
}

impl OutputBuffer {
    fn append(&mut self, bytes: &[u8]) {
        let total_len = self.bytes.len() + bytes.len();
        if total_len <= MAX_BUFFER_BYTES {
            self.bytes.extend_from_slice(bytes);
            return;
        }

        let head_len = MAX_BUFFER_BYTES / 2;
        let tail_len = MAX_BUFFER_BYTES - head_len;
        let old = std::mem::take(&mut self.bytes);
        self.bytes = Vec::with_capacity(MAX_BUFFER_BYTES);

        let old_head_len = head_len.min(old.len());
        self.bytes.extend_from_slice(&old[..old_head_len]);
        self.bytes
            .extend_from_slice(&bytes[..head_len - old_head_len]);

        if bytes.len() >= tail_len {
            self.bytes
                .extend_from_slice(&bytes[bytes.len() - tail_len..]);
        } else {
            self.bytes
                .extend_from_slice(&old[old.len() - (tail_len - bytes.len())..]);
            self.bytes.extend_from_slice(bytes);
        }
        self.omitted_bytes += total_len - MAX_BUFFER_BYTES;
    }

    fn append_output(&mut self, output: Self) {
        self.omitted_bytes += output.omitted_bytes;
        self.append(&output.bytes);
    }

    fn finish(self, full_output_path: PathBuf) -> CommandOutput {
        CommandOutput {
            content: String::from_utf8_lossy(&self.bytes).into_owned(),
            omitted_bytes: self.omitted_bytes,
            full_output_path,
        }
    }
}

#[derive(Default)]
struct TailBuffer {
    bytes: Vec<u8>,
    omitted_bytes: usize,
}

impl TailBuffer {
    fn append(&mut self, bytes: &[u8]) {
        let total_len = self.bytes.len() + bytes.len();
        if total_len <= MAX_TAIL_BYTES {
            self.bytes.extend_from_slice(bytes);
            return;
        }
        let omitted = total_len - MAX_TAIL_BYTES;
        self.omitted_bytes += omitted;
        if bytes.len() >= MAX_TAIL_BYTES {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&bytes[bytes.len() - MAX_TAIL_BYTES..]);
        } else {
            self.bytes.drain(..omitted);
            self.bytes.extend_from_slice(bytes);
        }
    }

    fn snapshot(&self) -> (String, usize) {
        (
            String::from_utf8_lossy(&self.bytes).into_owned(),
            self.omitted_bytes,
        )
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        let _ = kill_process_group(self.os_pid, Signal::KILL);
    }
}
