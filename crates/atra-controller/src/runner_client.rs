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
    CommandOutput, ProcessHandle, ProcessStatus, ProcessTiming, RunnerRequest,
    RunnerRequestEnvelope, RunnerResponse, RunnerResponseEnvelope, SpawnedProcess,
};
use atra_store::TreeManifest;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{Mutex, oneshot, watch},
};

pub(super) struct RunnerClient {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<RunnerResponse>>>>,
    subscriptions: Arc<StdMutex<HashMap<u64, watch::Sender<ProcessSubscriptionUpdate>>>>,
    next_request_id: Arc<AtomicU64>,
    name: String,
}

pub(super) struct ProcessSubscription {
    receiver: watch::Receiver<ProcessSubscriptionUpdate>,
}

#[derive(Clone)]
enum ProcessSubscriptionUpdate {
    Pending,
    Inspection(ProcessInspection),
    Error(String),
    Invalid,
}

impl ProcessSubscription {
    pub(super) async fn recv(&mut self) -> Result<ProcessInspection> {
        self.receiver
            .changed()
            .await
            .context("runner disconnected during process subscription")?;
        match self.receiver.borrow_and_update().clone() {
            ProcessSubscriptionUpdate::Pending => {
                bail!("runner returned an invalid process subscription response")
            }
            ProcessSubscriptionUpdate::Inspection(inspection) => Ok(inspection),
            ProcessSubscriptionUpdate::Error(message) => {
                bail!("Runner request failed: {message}")
            }
            ProcessSubscriptionUpdate::Invalid => {
                bail!("runner returned an invalid process subscription response")
            }
        }
    }
}

pub(super) enum PrepareTreeResult {
    MissingObjects(Vec<String>),
    Ready { digest: String, path: String },
}

#[derive(Clone)]
pub(super) struct ProcessInspection {
    pub(super) status: ProcessStatus,
    pub(super) output_tail: String,
    pub(super) omitted_bytes: usize,
}

pub(super) enum WaitOutcome {
    Running {
        process_handle: ProcessHandle,
        output: CommandOutput,
        patch_results: Vec<ApplyPatchResult>,
        spawned_processes: Vec<SpawnedProcess>,
        timing: ProcessTiming,
    },
    Finished {
        output: CommandOutput,
        exit_code: Option<i32>,
        patch_results: Vec<ApplyPatchResult>,
        spawned_processes: Vec<SpawnedProcess>,
    },
}

pub(super) struct StartedProcess {
    pub(super) handle: ProcessHandle,
    pub(super) timing: ProcessTiming,
}

impl RunnerClient {
    pub(super) fn new(stdin: ChildStdin, stdout: ChildStdout, name: &str) -> Self {
        let stdin = Arc::new(Mutex::new(stdin));
        let reader_stdin = Arc::clone(&stdin);
        let pending = Arc::new(StdMutex::new(
            HashMap::<u64, oneshot::Sender<RunnerResponse>>::new(),
        ));
        let reader_pending = Arc::clone(&pending);
        let subscriptions = Arc::new(StdMutex::new(HashMap::<
            u64,
            watch::Sender<ProcessSubscriptionUpdate>,
        >::new()));
        let reader_subscriptions = Arc::clone(&subscriptions);
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
                            let subscription = reader_subscriptions
                                .lock()
                                .unwrap()
                                .get(&envelope.request_id)
                                .cloned();
                            if let Some(subscription) = subscription {
                                let (update, terminal) = match envelope.response {
                                    RunnerResponse::ProcessInspected {
                                        process_status,
                                        output_tail,
                                        omitted_bytes,
                                    } => {
                                        let terminal =
                                            matches!(process_status, ProcessStatus::Exited { .. });
                                        (
                                            ProcessSubscriptionUpdate::Inspection(
                                                ProcessInspection {
                                                    status: process_status,
                                                    output_tail,
                                                    omitted_bytes,
                                                },
                                            ),
                                            terminal,
                                        )
                                    }
                                    RunnerResponse::Error { message } => {
                                        (ProcessSubscriptionUpdate::Error(message), true)
                                    }
                                    _ => (ProcessSubscriptionUpdate::Invalid, true),
                                };
                                if terminal {
                                    reader_subscriptions
                                        .lock()
                                        .unwrap()
                                        .remove(&envelope.request_id);
                                }
                                if subscription.send(update).is_err() {
                                    reader_subscriptions
                                        .lock()
                                        .unwrap()
                                        .remove(&envelope.request_id);
                                }
                                continue;
                            }
                            let sender =
                                reader_pending.lock().unwrap().remove(&envelope.request_id);
                            if let Some(sender) = sender {
                                if let Err(RunnerResponse::ProcessStarted {
                                    process_handle, ..
                                }) = sender.send(envelope.response)
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
            reader_subscriptions.lock().unwrap().clear();
        });

        Self {
            stdin,
            pending,
            subscriptions,
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
        process_id: atra_protocol::ProcessId,
        process_prefix: String,
    ) -> Result<StartedProcess> {
        match self
            .request_raw(RunnerRequest::StartCommand {
                command,
                environment,
                process_id,
                process_prefix,
            })
            .await?
        {
            RunnerResponse::ProcessStarted {
                process_handle,
                timing,
            } => Ok(StartedProcess {
                handle: process_handle,
                timing,
            }),
            RunnerResponse::Error { message } => bail!("{message}"),
            _ => bail!("runner returned an invalid start_command response"),
        }
    }

    pub(super) async fn subscribe(
        &self,
        process_handle: ProcessHandle,
    ) -> Result<ProcessSubscription> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = watch::channel(ProcessSubscriptionUpdate::Pending);
        self.subscriptions
            .lock()
            .unwrap()
            .insert(request_id, sender);
        let mut request = serde_json::to_vec(&RunnerRequestEnvelope {
            request_id,
            request: RunnerRequest::SubscribeProcess { process_handle },
        })
        .context("failed to encode runner subscription request")?;
        request.push(b'\n');
        if let Err(error) = self.stdin.lock().await.write_all(&request).await {
            self.subscriptions.lock().unwrap().remove(&request_id);
            return Err(error)
                .with_context(|| format!("failed to subscribe to runner {}", self.name));
        }
        Ok(ProcessSubscription { receiver })
    }

    pub(super) async fn wait(
        &self,
        process_handle: ProcessHandle,
        active_timeout_ms: u64,
    ) -> Result<WaitOutcome> {
        match self
            .request_raw(RunnerRequest::WaitProcess {
                process_handle,
                active_timeout_ms,
            })
            .await?
        {
            RunnerResponse::ProcessRunning {
                process_handle,
                output,
                patch_results,
                spawned_processes,
                timing,
            } => Ok(WaitOutcome::Running {
                process_handle,
                output,
                patch_results,
                spawned_processes,
                timing,
            }),
            RunnerResponse::ProcessFinished {
                output,
                exit_code,
                patch_results,
                spawned_processes,
            } => Ok(WaitOutcome::Finished {
                output,
                exit_code,
                patch_results,
                spawned_processes,
            }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn process_subscription_keeps_only_the_latest_pending_state() {
        let (sender, receiver) = watch::channel(ProcessSubscriptionUpdate::Pending);
        let mut subscription = ProcessSubscription { receiver };
        sender
            .send(ProcessSubscriptionUpdate::Inspection(ProcessInspection {
                status: ProcessStatus::Running,
                output_tail: "first".to_owned(),
                omitted_bytes: 0,
            }))
            .unwrap();
        sender
            .send(ProcessSubscriptionUpdate::Inspection(ProcessInspection {
                status: ProcessStatus::Running,
                output_tail: "latest".to_owned(),
                omitted_bytes: 1,
            }))
            .unwrap();

        let latest = subscription.recv().await.unwrap();
        assert_eq!(latest.output_tail, "latest");
        assert_eq!(latest.omitted_bytes, 1);

        sender
            .send(ProcessSubscriptionUpdate::Inspection(ProcessInspection {
                status: ProcessStatus::Exited { exit_code: Some(0) },
                output_tail: "final".to_owned(),
                omitted_bytes: 2,
            }))
            .unwrap();
        drop(sender);

        let terminal = subscription.recv().await.unwrap();
        assert!(matches!(
            terminal.status,
            ProcessStatus::Exited { exit_code: Some(0) }
        ));
        assert_eq!(terminal.output_tail, "final");
        assert_eq!(terminal.omitted_bytes, 2);
        assert!(subscription.recv().await.is_err());
    }
}
