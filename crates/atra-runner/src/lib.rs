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
    CommandEnvironment, CommandOutput, ProcessHandle, ProcessStatus, RunnerRequest,
    RunnerRequestEnvelope, RunnerResponse, RunnerResponseEnvelope,
};
use atra_store::{PreparedTree, Store};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::{
    io::{self, AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{Mutex, Notify, watch},
    time::{Instant, sleep, sleep_until, timeout},
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
    let listener = UnixListener::bind(&processes.control_endpoint).with_context(|| {
        format!(
            "failed to bind Runner control socket {}",
            processes.control_endpoint.display()
        )
    })?;
    tokio::spawn(serve_control(listener, Arc::clone(&processes)));
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
        RunnerRequest::StartCommand {
            command,
            environment,
            process_id,
            process_prefix,
        } => {
            tracing::debug!(%command, "starting command");
            let process = processes
                .start(command, environment, process_id, process_prefix)
                .await?;
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

async fn serve_control(listener: UnixListener, processes: Arc<ProcessManager>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, "failed to accept Runner control connection");
                return;
            }
        };
        let processes = Arc::clone(&processes);
        tokio::spawn(async move {
            if let Err(error) = serve_control_connection(stream, &processes).await {
                tracing::warn!(error = %format!("{error:#}"), "Runner control request failed");
            }
        });
    }
}

async fn serve_control_connection(stream: UnixStream, processes: &ProcessManager) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .await
        .context("failed to read Runner control request")?
        == 0
    {
        bail!("Runner control client disconnected before sending a request");
    }
    let envelope: RunnerRequestEnvelope =
        serde_json::from_str(&line).context("failed to decode Runner control request")?;
    let response = match envelope.request {
        RunnerRequest::WaitProcess {
            process_handle,
            timeout_ms,
        } => {
            processes
                .wait(&process_handle, Duration::from_millis(timeout_ms))
                .await
        }
        RunnerRequest::StopProcess { process_handle } => processes.stop(&process_handle).await,
        _ => bail!("unsupported Runner control request"),
    }
    .unwrap_or_else(|error| RunnerResponse::Error {
        message: format!("{error:#}"),
    });
    let mut response = serde_json::to_vec(&RunnerResponseEnvelope {
        request_id: envelope.request_id,
        response,
    })
    .context("failed to encode Runner control response")?;
    response.push(b'\n');
    writer
        .write_all(&response)
        .await
        .context("failed to write Runner control response")
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
    processes: Mutex<HashMap<ProcessHandle, Arc<ManagedProcess>>>,
    output_directory: tempfile::TempDir,
    control_endpoint: PathBuf,
}

impl ProcessManager {
    fn new() -> Result<Self> {
        let output_directory =
            tempfile::tempdir().context("failed to create command output directory")?;
        let control_endpoint = output_directory.path().join("control.sock");
        Ok(Self {
            processes: Mutex::new(HashMap::new()),
            output_directory,
            control_endpoint,
        })
    }

    async fn start(
        &self,
        command: String,
        environment: CommandEnvironment,
        process_id: atra_protocol::ProcessId,
        process_prefix: String,
    ) -> Result<Arc<ManagedProcess>> {
        let handle = ProcessHandle(format!("{process_prefix}{process_id}"));
        let mut processes = self.processes.lock().await;
        if processes.contains_key(&handle) {
            bail!("process handle {handle} is already managed by this runner");
        }
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
        child.env("ATRI_RUNNER_ENDPOINT", &self.control_endpoint);
        child.env("ATRI_PROCESS_PREFIX", process_prefix);
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
        let full_output_path = self.output_directory.path().join(handle.as_ref());
        let full_output = std::fs::File::create(&full_output_path)
            .context("failed to create full command output file")?;
        let (output_shutdown, mut output_shutdown_requested) = watch::channel(false);
        let process = Arc::new(ManagedProcess {
            handle: handle.clone(),
            os_pid,
            child: Mutex::new(child),
            output: Mutex::new(OutputBuffer::default()),
            output_tail: Mutex::new(TailBuffer::default()),
            full_output_path,
            output_closed: AtomicBool::new(false),
            output_shutdown,
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
                let read = tokio::select! {
                    read = reader.read(&mut buffer) => read,
                    changed = output_shutdown_requested.changed() => {
                        if changed.is_err() || *output_shutdown_requested.borrow() {
                            break;
                        }
                        continue;
                    }
                };
                match read {
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

    async fn wait(&self, handle: &ProcessHandle, timeout: Duration) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let deadline = Instant::now() + timeout;
        loop {
            let output = process.take_output().await;
            if let Some(exit_code) = process.exit_code().await? {
                process.finish_output().await;
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
                    process_handle: handle.clone(),
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

    async fn stop(&self, handle: &ProcessHandle) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let _ = kill_process_group(process.os_pid, Signal::TERM);
        let deadline = Instant::now() + Duration::from_millis(200);
        let kill_deadline = deadline + Duration::from_secs(1);
        let mut killed = false;
        loop {
            if process.exit_code().await?.is_some() {
                break;
            }
            if !killed && Instant::now() >= deadline {
                let _ = kill_process_group(process.os_pid, Signal::KILL);
                killed = true;
            }
            if Instant::now() >= kill_deadline {
                bail!("process {handle} did not exit after SIGKILL");
            }
            sleep(Duration::from_millis(10)).await;
        }
        process.finish_output().await;
        let output = process.take_output().await;
        self.processes.lock().await.remove(handle);
        tracing::info!(process_handle = %handle, "process stopped");
        Ok(RunnerResponse::ProcessStopped {
            output: output.finish(process.full_output_path.clone()),
        })
    }

    async fn inspect(&self, handle: &ProcessHandle) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let process_status = match process.exit_code().await? {
            Some(exit_code) => {
                process.finish_output().await;
                ProcessStatus::Exited { exit_code }
            }
            None => ProcessStatus::Running,
        };
        let (output_tail, omitted_bytes) = process.output_tail.lock().await.snapshot();
        Ok(RunnerResponse::ProcessInspected {
            process_status,
            output_tail,
            omitted_bytes,
        })
    }

    async fn status(&self, handle: &ProcessHandle) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let process_status = match process.exit_code().await? {
            Some(exit_code) => {
                process.finish_output().await;
                ProcessStatus::Exited { exit_code }
            }
            None => ProcessStatus::Running,
        };
        Ok(RunnerResponse::ProcessStatus { process_status })
    }

    async fn process(&self, handle: &ProcessHandle) -> Result<Arc<ManagedProcess>> {
        self.processes
            .lock()
            .await
            .get(handle)
            .cloned()
            .with_context(|| format!("process handle {handle} is not managed by this runner"))
    }
}

struct ManagedProcess {
    handle: ProcessHandle,
    os_pid: Pid,
    child: Mutex<Child>,
    output: Mutex<OutputBuffer>,
    output_tail: Mutex<TailBuffer>,
    full_output_path: PathBuf,
    output_closed: AtomicBool,
    output_shutdown: watch::Sender<bool>,
    changed: Notify,
}

impl ManagedProcess {
    async fn take_output(&self) -> OutputBuffer {
        std::mem::take(&mut *self.output.lock().await)
    }

    async fn exit_code(&self) -> Result<Option<Option<i32>>> {
        let status = self
            .child
            .lock()
            .await
            .try_wait()
            .context("failed to inspect command process")?;
        Ok(status.map(|status| status.code()))
    }

    async fn finish_output(&self) {
        if self.output_closed.load(Ordering::Acquire) {
            return;
        }
        let drained = timeout(Duration::from_millis(50), async {
            while !self.output_closed.load(Ordering::Acquire) {
                tokio::select! {
                    () = self.changed.notified() => {}
                    () = sleep(Duration::from_millis(10)) => {}
                }
            }
        })
        .await
        .is_ok();
        if !drained {
            self.output_shutdown.send_replace(true);
            if timeout(Duration::from_secs(1), async {
                while !self.output_closed.load(Ordering::Acquire) {
                    tokio::select! {
                        () = self.changed.notified() => {}
                        () = sleep(Duration::from_millis(10)) => {}
                    }
                }
            })
            .await
            .is_err()
            {
                tracing::warn!(
                    process_handle = %self.handle,
                    "timed out while stopping process output reader"
                );
            }
        }
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
        self.output_shutdown.send_replace(true);
        let _ = kill_process_group(self.os_pid, Signal::KILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_does_not_wait_for_inherited_output_descriptors() {
        let processes = ProcessManager::new().unwrap();
        let process = processes
            .start(
                "setsid sleep 2 &".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("process".to_owned()),
                "test-".to_owned(),
            )
            .await
            .unwrap();
        sleep(Duration::from_millis(100)).await;

        let response = timeout(Duration::from_secs(1), processes.stop(&process.handle))
            .await
            .expect("stop waited for an escaped child's output descriptors")
            .unwrap();

        assert!(matches!(response, RunnerResponse::ProcessStopped { .. }));
    }
}
