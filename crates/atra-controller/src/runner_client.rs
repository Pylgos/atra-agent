use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use atra_patch::ApplyPatchResult;
use atra_protocol::{
    CommandOutput, ProcessHandle, ProcessStatus, RunnerRequest, RunnerRequestEnvelope,
    RunnerResponse, RunnerResponseEnvelope,
};
use atra_store::TreeManifest;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{Mutex, oneshot},
};

pub(super) struct RunnerClient {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<RunnerResponse>>>>,
    next_request_id: Arc<AtomicU64>,
    name: String,
}

pub(super) enum PrepareTreeResult {
    MissingObjects(Vec<String>),
    Ready { digest: String, path: String },
}

pub(super) struct ProcessInspection {
    pub(super) status: ProcessStatus,
    pub(super) output_tail: String,
    pub(super) omitted_bytes: usize,
}

pub(super) enum WaitOutcome {
    Running {
        process_handle: ProcessHandle,
        output: CommandOutput,
    },
    Finished {
        output: CommandOutput,
        exit_code: Option<i32>,
    },
}

impl RunnerClient {
    pub(super) fn new(stdin: ChildStdin, stdout: ChildStdout, name: &str) -> Self {
        let stdin = Arc::new(Mutex::new(stdin));
        let reader_stdin = Arc::clone(&stdin);
        let pending = Arc::new(StdMutex::new(
            HashMap::<u64, oneshot::Sender<RunnerResponse>>::new(),
        ));
        let reader_pending = Arc::clone(&pending);
        let next_request_id = Arc::new(AtomicU64::new(0));
        let reader_next_request_id = Arc::clone(&next_request_id);
        let runner_name = name.to_owned();
        tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match stdout.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => match serde_json::from_str::<RunnerResponseEnvelope>(&line) {
                        Ok(envelope) => {
                            let sender =
                                reader_pending.lock().unwrap().remove(&envelope.request_id);
                            if let Some(sender) = sender {
                                if let Err(RunnerResponse::ProcessStarted { process_handle }) =
                                    sender.send(envelope.response)
                                {
                                    let request_id =
                                        reader_next_request_id.fetch_add(1, Ordering::Relaxed);
                                    let mut request = serde_json::to_vec(&RunnerRequestEnvelope {
                                        request_id,
                                        request: RunnerRequest::StopProcess { process_handle },
                                    })
                                    .expect("runner stop request should encode");
                                    request.push(b'\n');
                                    if let Err(error) =
                                        reader_stdin.lock().await.write_all(&request).await
                                    {
                                        tracing::warn!(
                                            runner = runner_name,
                                            %error,
                                            "failed to stop an abandoned process"
                                        );
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    runner = runner_name,
                                    request_id = envelope.request_id,
                                    "runner returned an unknown request ID"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                runner = runner_name,
                                %error,
                                "runner returned an invalid response"
                            );
                            break;
                        }
                    },
                    Err(error) => {
                        tracing::warn!(
                            runner = runner_name,
                            %error,
                            "failed to read runner response"
                        );
                        break;
                    }
                }
            }
            for (_, sender) in reader_pending.lock().unwrap().drain() {
                let _ = sender.send(RunnerResponse::Error {
                    message: format!("runner {runner_name} disconnected"),
                });
            }
        });

        Self {
            stdin,
            pending,
            next_request_id,
            name: name.to_owned(),
        }
    }

    pub(super) async fn initialize(&self) -> Result<()> {
        match self.request_raw(RunnerRequest::Initialize).await? {
            RunnerResponse::Ready => Ok(()),
            response => bail!("runner returned an invalid readiness response: {response:?}"),
        }
    }

    pub(super) async fn start(
        &self,
        command: String,
        environment: atra_protocol::CommandEnvironment,
    ) -> Result<ProcessHandle> {
        match self
            .request_raw(RunnerRequest::StartCommand {
                command,
                environment,
            })
            .await?
        {
            RunnerResponse::ProcessStarted { process_handle } => Ok(process_handle),
            RunnerResponse::Error { message } => bail!("{message}"),
            _ => bail!("runner returned an invalid start_command response"),
        }
    }

    pub(super) async fn wait(
        &self,
        process_handle: ProcessHandle,
        timeout_ms: u64,
    ) -> Result<WaitOutcome> {
        match self
            .request_raw(RunnerRequest::WaitProcess {
                process_handle,
                timeout_ms,
            })
            .await?
        {
            RunnerResponse::ProcessRunning {
                process_handle,
                output,
            } => Ok(WaitOutcome::Running {
                process_handle,
                output,
            }),
            RunnerResponse::ProcessFinished { output, exit_code } => {
                Ok(WaitOutcome::Finished { output, exit_code })
            }
            RunnerResponse::Error { message } => bail!("{message}"),
            _ => bail!("runner returned an invalid wait_process response"),
        }
    }

    pub(super) async fn stop(&self, process_handle: ProcessHandle) -> Result<CommandOutput> {
        match self
            .request_raw(RunnerRequest::StopProcess { process_handle })
            .await?
        {
            RunnerResponse::ProcessStopped { output } => Ok(output),
            RunnerResponse::Error { message } => bail!("{message}"),
            _ => bail!("runner returned an invalid stop_process response"),
        }
    }

    pub(super) async fn inspect(&self, process_handle: ProcessHandle) -> Result<ProcessInspection> {
        match self
            .request_raw(RunnerRequest::InspectProcess { process_handle })
            .await?
        {
            RunnerResponse::ProcessInspected {
                process_status,
                output_tail,
                omitted_bytes,
            } => Ok(ProcessInspection {
                status: process_status,
                output_tail,
                omitted_bytes,
            }),
            RunnerResponse::Error { message } => bail!("{message}"),
            _ => bail!("runner returned an invalid inspect_process response"),
        }
    }

    pub(super) async fn status(&self, process_handle: ProcessHandle) -> Result<ProcessStatus> {
        match self
            .request_raw(RunnerRequest::ProcessStatus { process_handle })
            .await?
        {
            RunnerResponse::ProcessStatus { process_status } => Ok(process_status),
            RunnerResponse::Error { message } => bail!("{message}"),
            _ => bail!("runner returned an invalid process_status response"),
        }
    }

    pub(super) async fn apply_patch(&self, patch: String) -> Result<ApplyPatchResult> {
        match self
            .request_raw(RunnerRequest::ApplyPatch { patch })
            .await?
        {
            RunnerResponse::PatchCompleted { result } => Ok(result),
            RunnerResponse::Error { message } => bail!("{message}"),
            _ => bail!("runner returned an invalid apply_patch response"),
        }
    }

    pub(super) async fn prepare_tree(&self, manifest: TreeManifest) -> Result<PrepareTreeResult> {
        match self
            .request_raw(RunnerRequest::PrepareTree { manifest })
            .await?
        {
            RunnerResponse::MissingObjects { digests } => {
                Ok(PrepareTreeResult::MissingObjects(digests))
            }
            RunnerResponse::TreeReady { digest, path } => {
                Ok(PrepareTreeResult::Ready { digest, path })
            }
            response => bail!("runner returned an invalid tree response: {response:?}"),
        }
    }

    pub(super) async fn upload_object(
        &self,
        digest: String,
        executable: bool,
        blob: String,
    ) -> Result<()> {
        match self
            .request_raw(RunnerRequest::UploadObject {
                digest,
                executable,
                blob,
            })
            .await?
        {
            RunnerResponse::ObjectStored => Ok(()),
            response => bail!("runner returned an invalid object response: {response:?}"),
        }
    }

    pub(super) async fn request_raw(&self, request: RunnerRequest) -> Result<RunnerResponse> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(request_id, sender);

        let mut request = serde_json::to_vec(&RunnerRequestEnvelope {
            request_id,
            request,
        })
        .context("failed to encode runner request")?;
        request.push(b'\n');
        if let Err(error) = self.stdin.lock().await.write_all(&request).await {
            self.pending.lock().unwrap().remove(&request_id);
            return Err(error)
                .with_context(|| format!("failed to send request to runner {}", self.name));
        }

        receiver
            .await
            .with_context(|| format!("runner {} disconnected", self.name))
    }
}
