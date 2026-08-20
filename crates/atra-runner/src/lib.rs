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
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_patch::apply;
use atra_protocol::{
    AgentControlRequestEnvelope, AgentControlResponseEnvelope, AgentResponse, CommandEnvironment,
    CommandOutput, ControllerRunnerMessage, ProcessHandle, ProcessStatus, ProcessTiming,
    RunnerCallbackCancelEnvelope, RunnerCallbackRequestEnvelope, RunnerControllerMessage,
    RunnerRequest, RunnerRequestEnvelope, RunnerResponse, RunnerResponseEnvelope, SpawnedProcess,
};
use atra_store::{PreparedTree, Store};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::{
    io::{
        self, AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{Mutex, Notify, oneshot, watch},
    time::{Instant, sleep, sleep_until, timeout},
};

mod workspace;

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
    let envelope =
        match serde_json::from_str(&line).context("failed to decode runner initialize request")? {
            ControllerRunnerMessage::Request(envelope) => envelope,
            ControllerRunnerMessage::CallbackResponse(_) => {
                bail!("runner received a callback response before initialization")
            }
        };
    let writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
        Arc::new(Mutex::new(Box::new(writer)));
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

    let callbacks = Arc::new(CallbackBridge::new(Arc::clone(&writer)));
    let processes = Arc::new(ProcessManager::new()?);
    let listener = UnixListener::bind(&processes.control_endpoint).with_context(|| {
        format!(
            "failed to bind Runner control socket {}",
            processes.control_endpoint.display()
        )
    })?;
    tokio::spawn(serve_control(
        listener,
        Arc::clone(&processes),
        Arc::clone(&callbacks),
    ));
    loop {
        line.clear();
        let bytes = match reader.read_line(&mut line).await {
            Ok(bytes) => bytes,
            Err(error) => {
                callbacks.close().await;
                return Err(error).context("failed to read runner request");
            }
        };
        if bytes == 0 {
            callbacks.close().await;
            return Ok(());
        }
        let message: ControllerRunnerMessage = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                callbacks.close().await;
                return Err(error).context("failed to decode runner request");
            }
        };
        let envelope = match message {
            ControllerRunnerMessage::Request(envelope) => envelope,
            ControllerRunnerMessage::CallbackResponse(envelope) => {
                callbacks
                    .complete(envelope.callback_id, envelope.response)
                    .await;
                continue;
            }
        };
        let writer = Arc::clone(&writer);
        let processes = Arc::clone(&processes);
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            let request_id = envelope.request_id;
            let request = match envelope.request {
                RunnerRequest::SubscribeProcess { process_handle } => {
                    if let Err(error) = processes
                        .subscribe(&process_handle, request_id, &writer)
                        .await
                    {
                        let response = RunnerResponse::Error {
                            message: format!("{error:#}"),
                        };
                        if let Err(error) = write_response(
                            &writer,
                            RunnerResponseEnvelope {
                                request_id,
                                response,
                            },
                        )
                        .await
                        {
                            tracing::warn!(
                                error = %format!("{error:#}"),
                                "failed to send runner subscription error"
                            );
                        }
                    }
                    return;
                }
                request => request,
            };
            let response = match handle_request(&processes, &store, request).await {
                Ok(response) => response,
                Err(error) => RunnerResponse::Error {
                    message: format!("{error:#}"),
                },
            };
            if let Err(error) = write_response(
                &writer,
                RunnerResponseEnvelope {
                    request_id,
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
            execution_context,
        } => {
            tracing::debug!(%command, "starting command");
            let process = processes
                .start_with_context(
                    command,
                    environment,
                    process_id,
                    process_prefix,
                    execution_context,
                    None,
                )
                .await?;
            Ok(RunnerResponse::ProcessStarted {
                process_handle: process.handle.clone(),
                timing: process.timing(),
            })
        }
        RunnerRequest::SpawnProcess { .. } => {
            bail!("processes can only be spawned through the Runner control socket")
        }
        RunnerRequest::ApplyPatch {
            process_handle,
            cwd,
            patch,
        } => processes.apply_patch(&process_handle, cwd, patch).await,
        RunnerRequest::ReplaceText {
            process_handle,
            cwd,
            path,
            old,
            new,
            replace_all,
        } => {
            processes
                .replace_text(&process_handle, cwd, path, old, new, replace_all)
                .await
        }
        RunnerRequest::WaitProcess {
            process_handle,
            active_timeout_ms,
        } => {
            processes
                .wait(
                    &process_handle,
                    Duration::from_millis(active_timeout_ms),
                    Duration::from_secs(1),
                )
                .await
        }
        RunnerRequest::WaitChildProcess { .. } => {
            bail!("child processes can only be waited through the Runner control socket")
        }
        RunnerRequest::SubscribeProcess { .. } => {
            bail!("process subscriptions must be handled by the runner connection")
        }
        RunnerRequest::StopProcess { process_handle } => processes.stop(&process_handle).await,
        RunnerRequest::InspectProcess { process_handle } => {
            processes.inspect(&process_handle).await
        }
        RunnerRequest::ProcessStatus { process_handle } => processes.status(&process_handle).await,
        RunnerRequest::Query { query } => Ok(RunnerResponse::Query {
            response: workspace::handle(query).await,
        }),
    }
}

async fn serve_control(
    listener: UnixListener,
    processes: Arc<ProcessManager>,
    callbacks: Arc<CallbackBridge>,
) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, "failed to accept Runner control connection");
                return;
            }
        };
        let processes = Arc::clone(&processes);
        let callbacks = Arc::clone(&callbacks);
        tokio::spawn(async move {
            if let Err(error) = serve_control_connection(stream, &processes, Some(&callbacks)).await
            {
                tracing::warn!(error = %format!("{error:#}"), "Runner control request failed");
            }
        });
    }
}

async fn serve_control_connection(
    stream: UnixStream,
    processes: &ProcessManager,
    callbacks: Option<&CallbackBridge>,
) -> Result<()> {
    let peer_pid = stream
        .peer_cred()
        .context("failed to read Runner control peer credentials")?
        .pid()
        .context("Runner control peer did not provide a PID")?;
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
    if let Ok(envelope) = serde_json::from_str::<AgentControlRequestEnvelope>(&line) {
        let callbacks =
            callbacks.context("agent operations are unavailable on this Runner connection")?;
        let process = processes.process(&envelope.process_handle).await?;
        process.authorize_control_peer(peer_pid)?;
        let pause_timer = matches!(&envelope.request, atra_protocol::AgentRequest::Wait { .. });
        let (callback_id, response) = callbacks
            .begin(process.execution_context.clone(), envelope.request)
            .await?;
        let response = relay_agent_callback(
            &mut reader,
            &process,
            callbacks,
            callback_id,
            response,
            pause_timer,
        )
        .await?;
        let Some(response) = response else {
            return Ok(());
        };
        let mut encoded = serde_json::to_vec(&AgentControlResponseEnvelope {
            request_id: envelope.request_id,
            response,
        })
        .context("failed to encode agent control response")?;
        encoded.push(b'\n');
        writer
            .write_all(&encoded)
            .await
            .context("failed to write agent control response")?;
        return Ok(());
    }
    let envelope: RunnerRequestEnvelope =
        serde_json::from_str(&line).context("failed to decode Runner control request")?;
    let response = match envelope.request {
        RunnerRequest::WaitChildProcess {
            waiting_process_handle,
            process_handle,
            timeout_ms,
        } => {
            let mut disconnected = [0];
            tokio::select! {
                response = processes.wait_for_child(
                    &waiting_process_handle,
                    &process_handle,
                    Duration::from_millis(timeout_ms),
                ) => response,
                read = reader.read(&mut disconnected) => {
                    match read.context("failed to monitor Runner control client")? {
                        0 => return Ok(()),
                        _ => bail!("Runner control client sent data after its request"),
                    }
                }
            }
        }
        RunnerRequest::WaitProcess { .. } => {
            bail!("controller process waits are not supported by the Runner control socket")
        }
        RunnerRequest::SubscribeProcess { .. } => {
            bail!("process subscriptions are not supported by the Runner control socket")
        }
        RunnerRequest::StopProcess { process_handle } => processes.stop(&process_handle).await,
        RunnerRequest::SpawnProcess {
            parent_process_handle,
            command,
            cwd,
            environment,
            process_id,
        } => {
            processes
                .spawn(
                    &parent_process_handle,
                    command,
                    cwd,
                    environment,
                    process_id,
                )
                .await
        }
        RunnerRequest::ApplyPatch {
            process_handle,
            cwd,
            patch,
        } => processes.apply_patch(&process_handle, cwd, patch).await,
        RunnerRequest::ReplaceText {
            process_handle,
            cwd,
            path,
            old,
            new,
            replace_all,
        } => {
            processes
                .replace_text(&process_handle, cwd, path, old, new, replace_all)
                .await
        }
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

async fn relay_agent_callback(
    reader: &mut (impl AsyncRead + Unpin),
    process: &Arc<ManagedProcess>,
    callbacks: &CallbackBridge,
    callback_id: u64,
    mut response: oneshot::Receiver<AgentResponse>,
    pause_timer: bool,
) -> Result<Option<AgentResponse>> {
    enum Terminal {
        Response(AgentResponse),
        Cancel(Option<anyhow::Error>),
    }

    let terminal = {
        let _active_wait = pause_timer.then(|| process.begin_wait());
        let mut disconnected = [0];
        tokio::select! {
            response = &mut response => Terminal::Response(
                response.context("Controller disconnected during agent request")?
            ),
            read = reader.read(&mut disconnected) => {
                if callbacks.claim_cancel(callback_id).await {
                    Terminal::Cancel(
                        read.err().map(|error| {
                            anyhow::Error::new(error)
                                .context("failed to monitor agent control client")
                        })
                    )
                } else {
                    Terminal::Response(
                        response.await.context("Controller disconnected during agent request")?
                    )
                }
            }
            result = process.wait_until_exit() => {
                if callbacks.claim_cancel(callback_id).await {
                    Terminal::Cancel(result.err())
                } else {
                    Terminal::Response(
                        response.await.context("Controller disconnected during agent request")?
                    )
                }
            }
        }
    };

    match terminal {
        Terminal::Response(response) => Ok(Some(response)),
        Terminal::Cancel(error) => {
            callbacks.send_cancel(callback_id).await;
            match error {
                Some(error) => Err(error),
                None => Ok(None),
            }
        }
    }
}

async fn write_response(
    writer: &Mutex<impl AsyncWrite + Unpin>,
    response: RunnerResponseEnvelope,
) -> Result<()> {
    let mut response = serde_json::to_vec(&RunnerControllerMessage::Response(response))
        .context("failed to encode runner response")?;
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

struct CallbackBridge {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<AgentResponse>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

impl CallbackBridge {
    fn new(writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>) -> Self {
        Self {
            writer,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }
    }

    async fn begin(
        &self,
        execution_context: String,
        request: atra_protocol::AgentRequest,
    ) -> Result<(u64, oneshot::Receiver<AgentResponse>)> {
        if self.closed.load(Ordering::Acquire) {
            bail!("Controller disconnected");
        }
        let callback_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        let mut pending = self.pending.lock().await;
        if self.closed.load(Ordering::Acquire) {
            bail!("Controller disconnected");
        }
        pending.insert(callback_id, sender);
        drop(pending);
        let message = RunnerControllerMessage::CallbackRequest(RunnerCallbackRequestEnvelope {
            callback_id,
            execution_context,
            request,
        });
        let mut encoded =
            serde_json::to_vec(&message).context("failed to encode agent callback")?;
        encoded.push(b'\n');
        if let Err(error) = self.writer.lock().await.write_all(&encoded).await {
            self.pending.lock().await.remove(&callback_id);
            return Err(error).context("failed to send agent callback");
        }
        Ok((callback_id, receiver))
    }

    async fn complete(&self, callback_id: u64, response: AgentResponse) {
        if let Some(sender) = self.pending.lock().await.remove(&callback_id) {
            let _ = sender.send(response);
        }
    }

    #[cfg(test)]
    async fn cancel(&self, callback_id: u64) {
        if !self.claim_cancel(callback_id).await {
            return;
        }
        self.send_cancel(callback_id).await;
    }

    async fn claim_cancel(&self, callback_id: u64) -> bool {
        self.pending.lock().await.remove(&callback_id).is_some()
    }

    async fn send_cancel(&self, callback_id: u64) {
        let message =
            RunnerControllerMessage::CallbackCancel(RunnerCallbackCancelEnvelope { callback_id });
        if let Ok(mut encoded) = serde_json::to_vec(&message) {
            encoded.push(b'\n');
            let _ = self.writer.lock().await.write_all(&encoded).await;
        }
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.pending.lock().await.clear();
    }
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

    #[cfg(test)]
    async fn start(
        &self,
        command: String,
        environment: CommandEnvironment,
        process_id: atra_protocol::ProcessId,
        process_prefix: String,
        cwd: Option<PathBuf>,
    ) -> Result<Arc<ManagedProcess>> {
        self.start_with_context(
            command,
            environment,
            process_id,
            process_prefix,
            String::new(),
            cwd,
        )
        .await
    }

    async fn start_with_context(
        &self,
        command: String,
        environment: CommandEnvironment,
        process_id: atra_protocol::ProcessId,
        process_prefix: String,
        execution_context: String,
        cwd: Option<PathBuf>,
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
        if let Some(cwd) = cwd {
            child.current_dir(cwd);
        }
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
        child.env("ATRI_PROCESS_PREFIX", &process_prefix);
        child.env("ATRI_PROCESS_HANDLE", handle.as_ref());
        let child = child
            .spawn()
            .context("failed to execute command with bash")?;
        let started_at = Instant::now();
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
            patch_results: Mutex::new(Vec::new()),
            spawned_processes: Mutex::new(Vec::new()),
            process_prefix,
            execution_context,
            patching: Mutex::new(()),
            full_output_path,
            output_closed: AtomicBool::new(false),
            timing: StdMutex::new(ProcessClock::new(started_at)),
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

    async fn spawn(
        &self,
        parent_handle: &ProcessHandle,
        command: String,
        cwd: PathBuf,
        environment: CommandEnvironment,
        process_id: atra_protocol::ProcessId,
    ) -> Result<RunnerResponse> {
        let parent = self.process(parent_handle).await?;
        let process = self
            .start_with_context(
                command.clone(),
                environment,
                process_id.clone(),
                parent.process_prefix.clone(),
                parent.execution_context.clone(),
                Some(cwd),
            )
            .await?;
        parent.spawned_processes.lock().await.push(SpawnedProcess {
            process_id,
            process_handle: process.handle.clone(),
            command,
        });
        Ok(RunnerResponse::ProcessStarted {
            process_handle: process.handle.clone(),
            timing: process.timing(),
        })
    }

    async fn subscribe(
        &self,
        handle: &ProcessHandle,
        request_id: u64,
        writer: &Mutex<impl AsyncWrite + Unpin>,
    ) -> Result<()> {
        let process = self.process(handle).await?;
        let mut previous = None;
        loop {
            let process_status = match process.exit_code().await? {
                Some(exit_code) => {
                    process.finish_output().await;
                    ProcessStatus::Exited { exit_code }
                }
                None => ProcessStatus::Running,
            };
            let (output_tail, omitted_bytes) = process.output_tail.lock().await.snapshot();
            let state = (process_status, output_tail, omitted_bytes);
            let finished = matches!(state.0, ProcessStatus::Exited { .. });
            if previous.as_ref() != Some(&state) {
                write_response(
                    writer,
                    RunnerResponseEnvelope {
                        request_id,
                        response: RunnerResponse::ProcessInspected {
                            process_status: state.0.clone(),
                            output_tail: state.1.clone(),
                            omitted_bytes: state.2,
                        },
                    },
                )
                .await?;
                previous = Some(state);
            }
            if finished {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait(
        &self,
        handle: &ProcessHandle,
        active_timeout: Duration,
        poll_timeout: Duration,
    ) -> Result<RunnerResponse> {
        self.wait_inner(handle, poll_timeout, Some(active_timeout), true)
            .await
    }

    async fn wait_inner(
        &self,
        handle: &ProcessHandle,
        poll_timeout: Duration,
        active_timeout: Option<Duration>,
        return_on_progress: bool,
    ) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let deadline = Instant::now() + poll_timeout;
        let mut collected = OutputBuffer::default();
        loop {
            collected.append_output(process.take_output().await);
            if let Some(exit_code) = process.exit_code().await? {
                process.finish_output().await;
                collected.append_output(process.take_output().await);
                self.processes.lock().await.remove(handle);
                return Ok(RunnerResponse::ProcessFinished {
                    output: collected.finish(process.full_output_path.clone()),
                    exit_code,
                    patch_results: process.take_patch_results().await,
                    spawned_processes: process.take_spawned_processes().await,
                });
            }
            let (active_elapsed, timing) = process.timing_snapshot();
            if return_on_progress && (!collected.bytes.is_empty() || collected.omitted_bytes != 0)
                || active_timeout.is_some_and(|timeout| active_elapsed >= timeout)
                || Instant::now() >= deadline
            {
                return Ok(RunnerResponse::ProcessRunning {
                    process_handle: handle.clone(),
                    output: collected.finish(process.full_output_path.clone()),
                    patch_results: process.take_patch_results().await,
                    spawned_processes: process.take_spawned_processes().await,
                    timing,
                });
            }
            tokio::select! {
                () = process.changed.notified() => {}
                () = sleep_until(deadline) => {}
                () = sleep(Duration::from_millis(10)) => {}
            }
        }
    }

    async fn wait_for_child(
        &self,
        waiting_handle: &ProcessHandle,
        handle: &ProcessHandle,
        timeout: Duration,
    ) -> Result<RunnerResponse> {
        if waiting_handle == handle {
            bail!("process {waiting_handle} cannot wait for itself");
        }
        let waiting_process = self.process(waiting_handle).await?;
        let _active_wait = waiting_process.begin_wait();
        self.wait_inner(handle, timeout, None, false).await
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
        let _patching = process.patching.lock().await;
        let output = process.take_output().await;
        self.processes.lock().await.remove(handle);
        tracing::info!(process_handle = %handle, "process stopped");
        Ok(RunnerResponse::ProcessStopped {
            output: output.finish(process.full_output_path.clone()),
        })
    }

    async fn apply_patch(
        &self,
        handle: &ProcessHandle,
        cwd: PathBuf,
        patch: String,
    ) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let _patching = process.patching.lock().await;
        tracing::info!(process_handle = %handle, patch_bytes = patch.len(), "applying patch");
        tracing::trace!(%patch, "patch content");
        let result = tokio::task::spawn_blocking(move || apply(&patch, &cwd))
            .await
            .context("patch task failed")?;
        process.patch_results.lock().await.push(result.clone());
        Ok(RunnerResponse::PatchCompleted { result })
    }

    async fn replace_text(
        &self,
        handle: &ProcessHandle,
        cwd: PathBuf,
        path: PathBuf,
        old: String,
        new: String,
        replace_all: bool,
    ) -> Result<RunnerResponse> {
        let process = self.process(handle).await?;
        let _patching = process.patching.lock().await;
        tracing::info!(process_handle = %handle, path = %path.display(), "replacing text");
        let result = tokio::task::spawn_blocking(move || {
            atra_patch::replace(&path, &old, &new, replace_all, &cwd)
        })
        .await
        .context("replace task failed")?;
        process.patch_results.lock().await.push(result.clone());
        Ok(RunnerResponse::ReplaceCompleted { result })
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
    patch_results: Mutex<Vec<atra_patch::ApplyPatchResult>>,
    spawned_processes: Mutex<Vec<SpawnedProcess>>,
    process_prefix: String,
    execution_context: String,
    patching: Mutex<()>,
    full_output_path: PathBuf,
    output_closed: AtomicBool,
    timing: StdMutex<ProcessClock>,
    output_shutdown: watch::Sender<bool>,
    changed: Notify,
}

impl ManagedProcess {
    fn authorize_control_peer(&self, peer_pid: i32) -> Result<()> {
        let managed_pid = self.os_pid.as_raw_nonzero().get();
        if process_descends_from(peer_pid, managed_pid)? {
            return Ok(());
        }
        bail!(
            "Runner control peer {peer_pid} does not belong to managed process {}",
            self.handle
        )
    }

    fn begin_wait(self: &Arc<Self>) -> ActiveWait {
        if self.timing.lock().unwrap().begin_wait() {
            self.changed.notify_waiters();
        }
        ActiveWait {
            process: Arc::clone(self),
        }
    }

    #[cfg(test)]
    fn has_active_wait(&self) -> bool {
        self.timing.lock().unwrap().paused()
    }

    fn timing(&self) -> ProcessTiming {
        self.timing_snapshot().1
    }

    fn timing_snapshot(&self) -> (Duration, ProcessTiming) {
        let (active_elapsed, paused) = self.timing.lock().unwrap().snapshot();
        (
            active_elapsed,
            ProcessTiming {
                active_elapsed_ms: active_elapsed.as_millis().try_into().unwrap_or(u64::MAX),
                paused,
            },
        )
    }

    async fn take_output(&self) -> OutputBuffer {
        std::mem::take(&mut *self.output.lock().await)
    }

    async fn take_patch_results(&self) -> Vec<atra_patch::ApplyPatchResult> {
        std::mem::take(&mut *self.patch_results.lock().await)
    }

    async fn take_spawned_processes(&self) -> Vec<SpawnedProcess> {
        std::mem::take(&mut *self.spawned_processes.lock().await)
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

    async fn wait_until_exit(&self) -> Result<()> {
        loop {
            if self.exit_code().await?.is_some() {
                return Ok(());
            }
            tokio::select! {
                () = self.changed.notified() => {}
                () = sleep(Duration::from_millis(10)) => {}
            }
        }
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

fn process_descends_from(mut pid: i32, ancestor: i32) -> Result<bool> {
    let mut visited = 0;
    while pid > 0 && visited < 1024 {
        if pid == ancestor {
            return Ok(true);
        }
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
            .with_context(|| format!("failed to inspect Runner control peer process {pid}"))?;
        let after_name = stat
            .rfind(") ")
            .and_then(|end| stat.get(end + 2..))
            .context("invalid process stat")?;
        let parent = after_name
            .split_whitespace()
            .nth(1)
            .context("process stat is missing its parent PID")?
            .parse::<i32>()
            .context("process stat has an invalid parent PID")?;
        if parent == pid {
            break;
        }
        pid = parent;
        visited += 1;
    }
    Ok(false)
}

struct ActiveWait {
    process: Arc<ManagedProcess>,
}

impl Drop for ActiveWait {
    fn drop(&mut self) {
        if self.process.timing.lock().unwrap().end_wait() {
            self.process.changed.notify_waiters();
        }
    }
}

struct ProcessClock {
    active_elapsed: Duration,
    running_since: Option<Instant>,
    active_waits: usize,
}

impl ProcessClock {
    fn new(started_at: Instant) -> Self {
        Self {
            active_elapsed: Duration::ZERO,
            running_since: Some(started_at),
            active_waits: 0,
        }
    }

    fn begin_wait(&mut self) -> bool {
        let first = self.active_waits == 0;
        if first {
            let running_since = self
                .running_since
                .take()
                .expect("running process clock must have a start time");
            self.active_elapsed = self
                .active_elapsed
                .saturating_add(Instant::now().duration_since(running_since));
        }
        self.active_waits += 1;
        first
    }

    fn end_wait(&mut self) -> bool {
        assert!(self.active_waits > 0, "process wait count underflow");
        self.active_waits -= 1;
        let resumed = self.active_waits == 0;
        if resumed {
            self.running_since = Some(Instant::now());
        }
        resumed
    }

    fn paused(&self) -> bool {
        self.active_waits != 0
    }

    fn snapshot(&self) -> (Duration, bool) {
        let elapsed = self.running_since.map_or(self.active_elapsed, |started| {
            self.active_elapsed
                .saturating_add(Instant::now().duration_since(started))
        });
        (elapsed, self.paused())
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
    async fn callback_request_multiplexes_with_an_ordinary_response() {
        let (writer, reader) = tokio::io::duplex(4096);
        let writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(Mutex::new(Box::new(writer)));
        let bridge = CallbackBridge::new(Arc::clone(&writer));
        let (callback_id, response) = bridge
            .begin("context".into(), atra_protocol::AgentRequest::List)
            .await
            .unwrap();
        write_response(
            &writer,
            RunnerResponseEnvelope {
                request_id: 41,
                response: RunnerResponse::Ready,
            },
        )
        .await
        .unwrap();

        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<RunnerControllerMessage>(&line).unwrap(),
            RunnerControllerMessage::CallbackRequest(request)
                if request.callback_id == callback_id
        ));
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<RunnerControllerMessage>(&line).unwrap(),
            RunnerControllerMessage::Response(response) if response.request_id == 41
        ));

        bridge
            .complete(
                callback_id,
                AgentResponse {
                    output: "done".into(),
                    success: true,
                },
            )
            .await;
        assert_eq!(response.await.unwrap().output, "done");
    }

    #[tokio::test]
    async fn callback_response_and_late_cancel_are_first_terminal_wins() {
        let (writer, reader) = tokio::io::duplex(4096);
        let writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(Mutex::new(Box::new(writer)));
        let bridge = CallbackBridge::new(writer);
        let (id, response) = bridge
            .begin("context".into(), atra_protocol::AgentRequest::List)
            .await
            .unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let RunnerControllerMessage::CallbackRequest(request) =
            serde_json::from_str(&line).unwrap()
        else {
            panic!("expected callback request")
        };
        assert_eq!(request.callback_id, id);
        bridge
            .complete(
                id,
                AgentResponse {
                    output: "done".into(),
                    success: true,
                },
            )
            .await;
        assert_eq!(response.await.unwrap().output, "done");
        bridge.cancel(id).await;
        line.clear();
        assert!(
            timeout(Duration::from_millis(20), reader.read_line(&mut line))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn callback_cancel_is_idempotent_and_discards_late_response() {
        let (writer, mut reader) = tokio::io::duplex(4096);
        let writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(Mutex::new(Box::new(writer)));
        let bridge = CallbackBridge::new(writer);
        let (id, response) = bridge
            .begin("context".into(), atra_protocol::AgentRequest::List)
            .await
            .unwrap();
        let mut discard = vec![0; 4096];
        let _ = reader.read(&mut discard).await.unwrap();
        bridge.cancel(id).await;
        bridge.cancel(id).await;
        assert!(response.await.is_err());
        bridge
            .complete(
                id,
                AgentResponse {
                    output: "late".into(),
                    success: true,
                },
            )
            .await;
    }

    #[tokio::test]
    async fn callback_terminal_releases_wait_before_large_control_response_is_read() {
        let processes = ProcessManager::new().unwrap();
        let process = processes
            .start_with_context(
                "sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("callback-wait".to_owned()),
                "test-".to_owned(),
                "context".to_owned(),
                None,
            )
            .await
            .unwrap();
        let (callback_writer, callback_reader) = tokio::io::duplex(4096);
        let callback_writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(Mutex::new(Box::new(callback_writer)));
        let bridge = Arc::new(CallbackBridge::new(callback_writer));
        let (callback_id, response) = bridge
            .begin(
                "context".into(),
                atra_protocol::AgentRequest::Wait {
                    targets: Vec::new(),
                    timeout_ms: 1,
                },
            )
            .await
            .unwrap();
        let mut callback_reader = BufReader::new(callback_reader);
        let mut request = String::new();
        callback_reader.read_line(&mut request).await.unwrap();

        let (control, _unread_client) = UnixStream::pair().unwrap();
        let (reader, mut writer) = control.into_split();
        let mut reader = BufReader::new(reader);
        let relay_process = Arc::clone(&process);
        let relay_bridge = Arc::clone(&bridge);
        let relay = tokio::spawn(async move {
            let response = relay_agent_callback(
                &mut reader,
                &relay_process,
                &relay_bridge,
                callback_id,
                response,
                true,
            )
            .await
            .unwrap()
            .unwrap();
            let mut encoded = serde_json::to_vec(&AgentControlResponseEnvelope {
                request_id: 1,
                response,
            })
            .unwrap();
            encoded.push(b'\n');
            writer.write_all(&encoded).await.unwrap();
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while !process.has_active_wait() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("callback did not pause the process timer");
        bridge
            .complete(
                callback_id,
                AgentResponse {
                    output: "x".repeat(8 * 1024 * 1024),
                    success: true,
                },
            )
            .await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while process.has_active_wait() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal callback left the process timer paused");
        assert!(
            !relay.is_finished(),
            "large response unexpectedly completed without a control reader"
        );

        relay.abort();
        processes.stop(&process.handle).await.unwrap();
    }

    #[tokio::test]
    async fn control_disconnect_cancels_an_outstanding_callback() {
        let processes = ProcessManager::new().unwrap();
        let process = processes
            .start_with_context(
                "sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("callback-disconnect".to_owned()),
                "test-".to_owned(),
                "context".to_owned(),
                None,
            )
            .await
            .unwrap();
        let (callback_writer, callback_reader) = tokio::io::duplex(4096);
        let callback_writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(Mutex::new(Box::new(callback_writer)));
        let bridge = Arc::new(CallbackBridge::new(callback_writer));
        let (callback_id, response) = bridge
            .begin("context".into(), atra_protocol::AgentRequest::List)
            .await
            .unwrap();
        let mut callback_reader = BufReader::new(callback_reader);
        let mut line = String::new();
        callback_reader.read_line(&mut line).await.unwrap();

        let (control, client) = UnixStream::pair().unwrap();
        let (reader, _) = control.into_split();
        let mut reader = BufReader::new(reader);
        drop(client);
        assert!(
            relay_agent_callback(&mut reader, &process, &bridge, callback_id, response, false)
                .await
                .unwrap()
                .is_none()
        );
        line.clear();
        callback_reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<RunnerControllerMessage>(&line).unwrap(),
            RunnerControllerMessage::CallbackCancel(cancel) if cancel.callback_id == callback_id
        ));
        processes.stop(&process.handle).await.unwrap();
    }

    #[tokio::test]
    async fn process_exit_cancels_an_outstanding_callback() {
        let processes = ProcessManager::new().unwrap();
        let process = processes
            .start_with_context(
                "exit 0".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("callback-exit".to_owned()),
                "test-".to_owned(),
                "context".to_owned(),
                None,
            )
            .await
            .unwrap();
        let (callback_writer, callback_reader) = tokio::io::duplex(4096);
        let callback_writer: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(Mutex::new(Box::new(callback_writer)));
        let bridge = Arc::new(CallbackBridge::new(callback_writer));
        let (callback_id, response) = bridge
            .begin("context".into(), atra_protocol::AgentRequest::List)
            .await
            .unwrap();
        let mut callback_reader = BufReader::new(callback_reader);
        let mut line = String::new();
        callback_reader.read_line(&mut line).await.unwrap();

        let (control, _client) = UnixStream::pair().unwrap();
        let (reader, _) = control.into_split();
        let mut reader = BufReader::new(reader);
        assert!(
            relay_agent_callback(&mut reader, &process, &bridge, callback_id, response, false)
                .await
                .unwrap()
                .is_none()
        );
        line.clear();
        callback_reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<RunnerControllerMessage>(&line).unwrap(),
            RunnerControllerMessage::CallbackCancel(cancel) if cancel.callback_id == callback_id
        ));
    }

    #[tokio::test]
    async fn process_clock_excludes_active_wait_time() {
        let mut clock = ProcessClock::new(Instant::now());
        sleep(Duration::from_millis(20)).await;
        assert!(clock.begin_wait());
        let paused_elapsed = clock.snapshot().0;

        sleep(Duration::from_millis(20)).await;
        assert_eq!(clock.snapshot().0, paused_elapsed);

        assert!(clock.end_wait());
        sleep(Duration::from_millis(20)).await;
        assert!(clock.snapshot().0 > paused_elapsed);
    }

    #[tokio::test]
    async fn foreground_timeout_uses_runner_active_time() {
        let processes = ProcessManager::new().unwrap();
        let process = processes
            .start(
                "sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("process".to_owned()),
                "test-".to_owned(),
                None,
            )
            .await
            .unwrap();
        let active_timeout = process.timing_snapshot().0 + Duration::from_millis(30);
        let active_wait = process.begin_wait();

        let response = processes
            .wait(&process.handle, active_timeout, Duration::from_millis(50))
            .await
            .unwrap();
        let RunnerResponse::ProcessRunning { timing, .. } = response else {
            panic!("process unexpectedly finished");
        };
        assert!(timing.paused);
        assert!(process.timing_snapshot().0 < active_timeout);

        drop(active_wait);
        let response = timeout(
            Duration::from_millis(200),
            processes.wait(&process.handle, active_timeout, Duration::from_secs(1)),
        )
        .await
        .expect("active timeout did not expire")
        .unwrap();
        assert!(matches!(response, RunnerResponse::ProcessRunning { .. }));
        assert!(process.timing_snapshot().0 >= active_timeout);

        processes.stop(&process.handle).await.unwrap();
    }

    #[tokio::test]
    async fn subscription_reports_state_without_consuming_wait_output() {
        let processes = Arc::new(ProcessManager::new().unwrap());
        let process = processes
            .start(
                "printf subscribed; sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("process".to_owned()),
                "test-".to_owned(),
                None,
            )
            .await
            .unwrap();
        let handle = process.handle.clone();
        let (writer, reader) = tokio::io::duplex(64 * 1024);
        let writer = Arc::new(Mutex::new(writer));
        let subscription_processes = Arc::clone(&processes);
        let subscription_writer = Arc::clone(&writer);
        let subscription_handle = handle.clone();
        let subscription = tokio::spawn(async move {
            subscription_processes
                .subscribe(&subscription_handle, 7, &subscription_writer)
                .await
        });

        sleep(Duration::from_millis(50)).await;
        let wait = processes
            .wait(&handle, Duration::from_secs(120), Duration::from_millis(10))
            .await
            .unwrap();
        let RunnerResponse::ProcessRunning { output, .. } = wait else {
            panic!("process unexpectedly finished");
        };
        assert_eq!(output.content, "subscribed");

        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let RunnerControllerMessage::Response(snapshot) = serde_json::from_str(&line).unwrap()
        else {
            panic!("subscription returned a callback message")
        };
        let RunnerResponse::ProcessInspected {
            output_tail,
            process_status: ProcessStatus::Running,
            ..
        } = snapshot.response
        else {
            panic!("subscription did not start with a snapshot");
        };
        let mut output_tail = output_tail;
        while output_tail.is_empty() {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let RunnerControllerMessage::Response(envelope) = serde_json::from_str(&line).unwrap()
            else {
                panic!("subscription returned a callback message")
            };
            let RunnerResponse::ProcessInspected {
                output_tail: current,
                ..
            } = envelope.response
            else {
                panic!("subscription ended before delivering process output");
            };
            output_tail = current;
        }
        assert_eq!(output_tail, "subscribed");

        processes.stop(&handle).await.unwrap();
        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let RunnerControllerMessage::Response(envelope) = serde_json::from_str(&line).unwrap()
            else {
                panic!("subscription returned a callback message")
            };
            if matches!(
                envelope.response,
                RunnerResponse::ProcessInspected {
                    process_status: ProcessStatus::Exited { .. },
                    ..
                }
            ) {
                break;
            }
        }
        subscription.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stop_does_not_wait_for_inherited_output_descriptors() {
        let processes = ProcessManager::new().unwrap();
        let process = processes
            .start(
                "setsid sleep 2 &".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("process".to_owned()),
                "test-".to_owned(),
                None,
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

    #[tokio::test]
    async fn waiting_for_a_child_ignores_intermediate_output_and_marks_the_parent() {
        let processes = Arc::new(ProcessManager::new().unwrap());
        let parent = processes
            .start(
                "sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("parent".to_owned()),
                "test-".to_owned(),
                None,
            )
            .await
            .unwrap();
        let child = processes
            .start(
                "printf first; sleep 0.05; printf second; sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("child".to_owned()),
                "test-".to_owned(),
                None,
            )
            .await
            .unwrap();
        let wait_processes = Arc::clone(&processes);
        let parent_handle = parent.handle.clone();
        let child_handle = child.handle.clone();
        let wait = tokio::spawn(async move {
            wait_processes
                .wait_for_child(&parent_handle, &child_handle, Duration::from_millis(200))
                .await
        });

        timeout(Duration::from_secs(1), async {
            while !parent.has_active_wait() {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("child wait did not mark its parent");
        sleep(Duration::from_millis(100)).await;
        assert!(
            !wait.is_finished(),
            "child output ended the wait before its timeout"
        );
        let response = processes
            .wait(&parent.handle, Duration::from_secs(120), Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(
            response,
            RunnerResponse::ProcessRunning {
                timing: ProcessTiming { paused: true, .. },
                ..
            }
        ));

        let response = wait.await.unwrap().unwrap();
        let RunnerResponse::ProcessRunning { output, .. } = response else {
            panic!("child unexpectedly finished");
        };
        assert_eq!(output.content, "firstsecond");
        assert!(!parent.has_active_wait());

        processes.stop(&parent.handle).await.unwrap();
        processes.stop(&child.handle).await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_child_wait_unmarks_the_parent() {
        let processes = Arc::new(ProcessManager::new().unwrap());
        let parent = processes
            .start(
                "sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("parent".to_owned()),
                "test-".to_owned(),
                None,
            )
            .await
            .unwrap();
        let child = processes
            .start(
                "sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("child".to_owned()),
                "test-".to_owned(),
                None,
            )
            .await
            .unwrap();
        let wait_processes = Arc::clone(&processes);
        let parent_handle = parent.handle.clone();
        let child_handle = child.handle.clone();
        let wait = tokio::spawn(async move {
            wait_processes
                .wait_for_child(&parent_handle, &child_handle, Duration::from_secs(10))
                .await
        });

        timeout(Duration::from_secs(1), async {
            while !parent.has_active_wait() {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("child wait did not mark its parent");
        wait.abort();
        assert!(wait.await.unwrap_err().is_cancelled());
        assert!(!parent.has_active_wait());

        processes.stop(&parent.handle).await.unwrap();
        processes.stop(&child.handle).await.unwrap();
    }

    #[tokio::test]
    async fn process_cannot_wait_for_itself() {
        let processes = ProcessManager::new().unwrap();
        let process = processes
            .start(
                "sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("process".to_owned()),
                "test-".to_owned(),
                None,
            )
            .await
            .unwrap();

        let error = processes
            .wait_for_child(&process.handle, &process.handle, Duration::from_secs(10))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "process test-process cannot wait for itself"
        );
        assert!(!process.has_active_wait());
        processes.stop(&process.handle).await.unwrap();
    }

    #[tokio::test]
    async fn disconnecting_a_control_client_cancels_its_child_wait() {
        let processes = Arc::new(ProcessManager::new().unwrap());
        let parent = processes
            .start(
                "sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("parent".to_owned()),
                "test-".to_owned(),
                None,
            )
            .await
            .unwrap();
        let child = processes
            .start(
                "sleep 10".to_owned(),
                CommandEnvironment::default(),
                atra_protocol::ProcessId("child".to_owned()),
                "test-".to_owned(),
                None,
            )
            .await
            .unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let request = RunnerRequestEnvelope {
            request_id: 1,
            request: RunnerRequest::WaitChildProcess {
                waiting_process_handle: parent.handle.clone(),
                process_handle: child.handle.clone(),
                timeout_ms: 60_000,
            },
        };
        let mut message = serde_json::to_vec(&request).unwrap();
        message.push(b'\n');
        client.write_all(&message).await.unwrap();
        let connection_processes = Arc::clone(&processes);
        let connection = tokio::spawn(async move {
            serve_control_connection(server, &connection_processes, None).await
        });

        timeout(Duration::from_secs(1), async {
            while !parent.has_active_wait() {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("control request did not mark its parent");
        drop(client);
        timeout(Duration::from_secs(1), connection)
            .await
            .expect("control connection did not stop after disconnect")
            .unwrap()
            .unwrap();
        assert!(!parent.has_active_wait());

        processes.stop(&parent.handle).await.unwrap();
        processes.stop(&child.handle).await.unwrap();
    }

    #[test]
    fn process_ancestry_accepts_self_and_rejects_unrelated_pid() {
        let current = i32::try_from(std::process::id()).unwrap();
        assert!(process_descends_from(current, current).unwrap());
        assert!(!process_descends_from(current, i32::MAX).unwrap());
    }
}
