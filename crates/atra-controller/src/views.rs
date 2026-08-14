use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use atra_protocol::{
    CheckpointId, CheckpointState, CheckpointSubscriptionMessage, ControllerOperation,
    ControllerState, ControllerSubscriptionMessage, ProcessLocator, ProcessOperation, ProcessState,
    ProcessSubscriptionMessage, SubscriptionTerminal, ThreadId, ThreadOperation, ThreadState,
    ThreadSubscriptionMessage,
};
use tokio::sync::{Mutex, mpsc};

pub(super) struct Views {
    inner: Mutex<ViewStates>,
}

struct ViewStates {
    controller: ControllerView,
    threads: HashMap<ThreadId, ThreadView>,
    checkpoints: HashMap<CheckpointId, CheckpointView>,
    processes: HashMap<ProcessLocator, ProcessView>,
}

struct ControllerView {
    state: ControllerState,
    subscribers: Vec<mpsc::UnboundedSender<ControllerSubscriptionMessage>>,
}

struct ThreadView {
    state: ThreadState,
    subscribers: Vec<mpsc::UnboundedSender<ThreadSubscriptionMessage>>,
}

struct CheckpointView {
    state: CheckpointState,
    subscribers: Vec<mpsc::UnboundedSender<CheckpointSubscriptionMessage>>,
}

struct ProcessView {
    state: ProcessState,
    subscribers: Vec<mpsc::UnboundedSender<ProcessSubscriptionMessage>>,
}

impl Views {
    pub(super) fn new(controller: ControllerState) -> Self {
        Self {
            inner: Mutex::new(ViewStates {
                controller: ControllerView {
                    state: controller,
                    subscribers: Vec::new(),
                },
                threads: HashMap::new(),
                checkpoints: HashMap::new(),
                processes: HashMap::new(),
            }),
        }
    }

    pub(super) async fn has_thread(&self, thread_id: ThreadId) -> bool {
        self.inner.lock().await.threads.contains_key(&thread_id)
    }

    pub(super) async fn has_checkpoint(&self, checkpoint_id: CheckpointId) -> bool {
        self.inner
            .lock()
            .await
            .checkpoints
            .contains_key(&checkpoint_id)
    }

    pub(super) async fn has_process(&self, process: &ProcessLocator) -> bool {
        self.inner.lock().await.processes.contains_key(process)
    }

    pub(super) async fn ensure_running(&self) -> Result<()> {
        let inner = self.inner.lock().await;
        ensure_inner_running(&inner)
    }

    pub(super) async fn insert_thread(&self, state: ThreadState) {
        self.inner.lock().await.threads.insert(
            state.metadata().id,
            ThreadView {
                state,
                subscribers: Vec::new(),
            },
        );
    }

    pub(super) async fn insert_checkpoint(&self, state: CheckpointState) {
        self.inner.lock().await.checkpoints.insert(
            state.metadata().id,
            CheckpointView {
                state,
                subscribers: Vec::new(),
            },
        );
    }

    pub(super) async fn insert_process(&self, state: ProcessState) -> Result<()> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        let process = state.process().clone();
        let thread_id = process.locator().thread_id();
        let operation = ThreadOperation::ProcessUpdated { process };
        let thread = inner
            .threads
            .get_mut(&thread_id)
            .context("process thread state is not loaded")?;
        operation
            .clone()
            .apply(&mut thread.state)
            .context("failed to add process to thread state")?;
        let message = ThreadSubscriptionMessage::Operation { operation };
        thread
            .subscribers
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        inner.processes.insert(
            state.process().locator().clone(),
            ProcessView {
                state,
                subscribers: Vec::new(),
            },
        );
        Ok(())
    }

    pub(super) async fn subscribe_controller(
        &self,
    ) -> mpsc::UnboundedReceiver<ControllerSubscriptionMessage> {
        let mut inner = self.inner.lock().await;
        let (sender, receiver) = mpsc::unbounded_channel();
        sender
            .send(ControllerSubscriptionMessage::Snapshot {
                state: inner.controller.state.clone(),
            })
            .expect("new controller subscriber must be open");
        if inner_lifecycle(&inner) == atra_protocol::ControllerLifecycle::Stopping {
            sender
                .send(ControllerSubscriptionMessage::Terminal {
                    terminal: SubscriptionTerminal::ControllerShutdown,
                })
                .expect("new controller subscriber must be open");
            return receiver;
        }
        inner.controller.subscribers.push(sender);
        receiver
    }

    pub(super) async fn subscribe_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<mpsc::UnboundedReceiver<ThreadSubscriptionMessage>> {
        let mut inner = self.inner.lock().await;
        let view = inner
            .threads
            .get_mut(&thread_id)
            .context("thread state is not loaded")?;
        let (sender, receiver) = mpsc::unbounded_channel();
        sender
            .send(ThreadSubscriptionMessage::Snapshot {
                state: view.state.clone(),
            })
            .expect("new thread subscriber must be open");
        view.subscribers.push(sender);
        Ok(receiver)
    }

    pub(super) async fn subscribe_checkpoint(
        &self,
        checkpoint_id: CheckpointId,
    ) -> Result<mpsc::UnboundedReceiver<CheckpointSubscriptionMessage>> {
        let mut inner = self.inner.lock().await;
        let view = inner
            .checkpoints
            .get_mut(&checkpoint_id)
            .context("checkpoint state is not loaded")?;
        let (sender, receiver) = mpsc::unbounded_channel();
        sender
            .send(CheckpointSubscriptionMessage::Snapshot {
                state: view.state.clone(),
            })
            .expect("new checkpoint subscriber must be open");
        view.subscribers.push(sender);
        Ok(receiver)
    }

    pub(super) async fn subscribe_process(
        &self,
        process: &ProcessLocator,
    ) -> Result<mpsc::UnboundedReceiver<ProcessSubscriptionMessage>> {
        let mut inner = self.inner.lock().await;
        let view = inner
            .processes
            .get_mut(process)
            .context("process state is not loaded")?;
        let (sender, receiver) = mpsc::unbounded_channel();
        sender
            .send(ProcessSubscriptionMessage::Snapshot {
                state: view.state.clone(),
            })
            .expect("new process subscriber must be open");
        view.subscribers.push(sender);
        Ok(receiver)
    }

    pub(super) async fn apply_controller(&self, operation: ControllerOperation) -> Result<()> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        operation
            .clone()
            .apply(&mut inner.controller.state)
            .context("failed to apply controller operation")?;
        let message = ControllerSubscriptionMessage::Operation { operation };
        inner
            .controller
            .subscribers
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        Ok(())
    }

    pub(super) async fn start_provider_operation(
        &self,
        provider_id: &str,
        lifecycle: atra_protocol::ProviderLifecycle,
    ) -> Result<()> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        let current = inner
            .controller
            .state
            .providers()
            .iter()
            .find(|provider| provider.id() == provider_id)
            .context("provider does not exist in controller state")?;
        if matches!(
            current.lifecycle(),
            atra_protocol::ProviderLifecycle::LoggingIn
                | atra_protocol::ProviderLifecycle::LoggingOut
                | atra_protocol::ProviderLifecycle::Refreshing
        ) {
            bail!("provider already has an operation in progress");
        }
        let provider = atra_protocol::ProviderState::new(
            provider_id.to_owned(),
            lifecycle,
            current.models().to_vec(),
            current.rate_limits().cloned(),
        );
        let operation = ControllerOperation::ProviderUpdated { provider };
        operation
            .clone()
            .apply(&mut inner.controller.state)
            .context("failed to start provider operation")?;
        let message = ControllerSubscriptionMessage::Operation { operation };
        inner
            .controller
            .subscribers
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        Ok(())
    }

    pub(super) async fn start_runner_launch(&self, runner: atra_protocol::Runner) -> Result<()> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        if inner
            .controller
            .state
            .runners()
            .iter()
            .find(|current| current.runner().name == runner.name)
            .is_some_and(|current| {
                matches!(
                    current.lifecycle(),
                    atra_protocol::RunnerLifecycle::Launching
                )
            })
        {
            bail!("runner launch is already in progress");
        }
        let operation = ControllerOperation::RunnerUpdated {
            runner: atra_protocol::RunnerState::new(
                runner,
                atra_protocol::RunnerLifecycle::Launching,
            ),
        };
        operation
            .clone()
            .apply(&mut inner.controller.state)
            .context("failed to start Runner launch")?;
        let message = ControllerSubscriptionMessage::Operation { operation };
        inner
            .controller
            .subscribers
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        Ok(())
    }

    pub(super) async fn add_thread(&self, thread: atra_protocol::Thread) -> Result<()> {
        self.apply_controller(ControllerOperation::ThreadAdded { thread })
            .await
    }

    pub(super) async fn update_thread_metadata(
        &self,
        thread_id: ThreadId,
        metadata: atra_protocol::Thread,
    ) -> Result<()> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        let controller_operation = ControllerOperation::ThreadUpdated {
            thread: metadata.clone(),
        };
        let mut controller_state = inner.controller.state.clone();
        controller_operation
            .clone()
            .apply(&mut controller_state)
            .context("failed to update controller thread metadata")?;
        let thread_operation = ThreadOperation::MetadataUpdated { metadata };
        let mut thread_state = inner
            .threads
            .get(&thread_id)
            .context("thread state is not loaded")?
            .state
            .clone();
        thread_operation
            .clone()
            .apply(&mut thread_state)
            .context("failed to update thread metadata")?;
        inner.controller.state = controller_state;
        inner
            .threads
            .get_mut(&thread_id)
            .expect("thread state was cloned under the same lock")
            .state = thread_state;
        let controller_message = ControllerSubscriptionMessage::Operation {
            operation: controller_operation,
        };
        inner
            .controller
            .subscribers
            .retain(|subscriber| subscriber.send(controller_message.clone()).is_ok());
        let thread_message = ThreadSubscriptionMessage::Operation {
            operation: thread_operation,
        };
        inner
            .threads
            .get_mut(&thread_id)
            .expect("thread state was updated under the same lock")
            .subscribers
            .retain(|subscriber| subscriber.send(thread_message.clone()).is_ok());
        Ok(())
    }

    pub(super) async fn replace_thread_history(
        &self,
        thread_id: ThreadId,
        metadata: atra_protocol::Thread,
        events: Vec<atra_protocol::ThreadEvent>,
        checkpoint: atra_protocol::ThreadCheckpoint,
    ) -> Result<()> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        let controller_operation = ControllerOperation::ThreadUpdated {
            thread: metadata.clone(),
        };
        let thread_operations = [
            ThreadOperation::MetadataUpdated { metadata },
            ThreadOperation::EventsReplaced { events },
            ThreadOperation::CheckpointAdded { checkpoint },
        ];
        let mut controller_state = inner.controller.state.clone();
        controller_operation
            .clone()
            .apply(&mut controller_state)
            .context("failed to project replaced history metadata")?;
        let mut thread_state = inner
            .threads
            .get(&thread_id)
            .context("thread state is not loaded")?
            .state
            .clone();
        for operation in &thread_operations {
            operation
                .clone()
                .apply(&mut thread_state)
                .context("failed to project replaced thread history")?;
        }
        inner.controller.state = controller_state;
        inner
            .threads
            .get_mut(&thread_id)
            .expect("thread state was cloned under the same lock")
            .state = thread_state;
        let controller_message = ControllerSubscriptionMessage::Operation {
            operation: controller_operation,
        };
        inner
            .controller
            .subscribers
            .retain(|subscriber| subscriber.send(controller_message.clone()).is_ok());
        let thread = inner
            .threads
            .get_mut(&thread_id)
            .expect("thread state was replaced under the same lock");
        for operation in thread_operations {
            let message = ThreadSubscriptionMessage::Operation { operation };
            thread
                .subscribers
                .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        }
        Ok(())
    }

    pub(super) async fn apply_thread(
        &self,
        thread_id: ThreadId,
        operation: ThreadOperation,
    ) -> Result<()> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        let view = inner
            .threads
            .get_mut(&thread_id)
            .context("thread state is not loaded")?;
        operation
            .clone()
            .apply(&mut view.state)
            .context("failed to apply thread operation")?;
        let message = ThreadSubscriptionMessage::Operation { operation };
        view.subscribers
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        Ok(())
    }

    pub(super) async fn resolve_approval(
        &self,
        approval_id: atra_protocol::ApprovalId,
    ) -> Result<ThreadId> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        let thread_id = inner
            .threads
            .iter()
            .find_map(|(thread_id, view)| {
                view.state
                    .active_turn()
                    .and_then(|turn| turn.pending_approval())
                    .is_some_and(|approval| approval.id() == approval_id)
                    .then_some(*thread_id)
            })
            .context("approval is not pending in public state")?;
        let view = inner
            .threads
            .get_mut(&thread_id)
            .expect("approval thread was found in the same map");
        let operation = ThreadOperation::ApprovalResolved { approval_id };
        operation
            .clone()
            .apply(&mut view.state)
            .context("failed to resolve approval in public state")?;
        let message = ThreadSubscriptionMessage::Operation { operation };
        view.subscribers
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        Ok(thread_id)
    }

    pub(super) async fn start_cancellation(&self, thread_id: ThreadId) -> Result<()> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        let view = inner
            .threads
            .get_mut(&thread_id)
            .context("thread state is not loaded")?;
        let turn = view
            .state
            .active_turn()
            .context("thread has no active turn")?;
        if turn.phase() == atra_protocol::TurnPhase::Cancelling {
            bail!("thread cancellation is already in progress");
        }
        let operation = ThreadOperation::PhaseChanged {
            phase: atra_protocol::TurnPhase::Cancelling,
        };
        operation
            .clone()
            .apply(&mut view.state)
            .context("failed to start cancellation")?;
        let message = ThreadSubscriptionMessage::Operation { operation };
        view.subscribers
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        Ok(())
    }

    pub(super) async fn synchronize_process(
        &self,
        process: &ProcessLocator,
        output_tail: String,
        omitted_bytes: usize,
        status: atra_protocol::ProcessStatus,
    ) -> Result<bool> {
        let mut inner = self.inner.lock().await;
        ensure_inner_running(&inner)?;
        let current = inner
            .processes
            .get(process)
            .context("process state is not loaded")?;
        let mut process_state = current.state.clone();
        let mut process_operations = Vec::new();
        if process_state.output_tail() != output_tail
            || process_state.omitted_bytes() != omitted_bytes
        {
            let overlap = suffix_prefix_overlap(process_state.output_tail(), &output_tail);
            process_operations.push(ProcessOperation::OutputAppended {
                content: output_tail[overlap..].to_owned(),
                truncate_bytes: process_state.output_tail().len() - overlap,
                omitted_bytes,
            });
        }
        let status_changed = process_state.process().status() != &status;
        if status_changed {
            process_operations.push(ProcessOperation::StatusChanged {
                status: status.clone(),
            });
        }
        for operation in &process_operations {
            operation
                .clone()
                .apply(&mut process_state)
                .context("failed to synchronize process state")?;
        }
        let thread_id = process.thread_id();
        let mut thread_state = inner
            .threads
            .get(&thread_id)
            .context("process thread state is not loaded")?
            .state
            .clone();
        let thread_operation = status_changed.then(|| {
            let summary = process_state.process();
            ThreadOperation::ProcessUpdated {
                process: atra_protocol::ProcessSummary::new(
                    summary.locator().clone(),
                    summary.command().to_owned(),
                    summary.started_at_ms(),
                    status.clone(),
                ),
            }
        });
        if let Some(operation) = &thread_operation {
            operation
                .clone()
                .apply(&mut thread_state)
                .context("failed to synchronize thread process summary")?;
        }
        inner
            .processes
            .get_mut(process)
            .expect("process state was cloned under the same lock")
            .state = process_state;
        inner
            .threads
            .get_mut(&thread_id)
            .expect("thread state was cloned under the same lock")
            .state = thread_state;
        let process_view = inner
            .processes
            .get_mut(process)
            .expect("process state was synchronized under the same lock");
        for operation in process_operations {
            let message = ProcessSubscriptionMessage::Operation { operation };
            process_view
                .subscribers
                .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        }
        if let Some(operation) = thread_operation {
            let message = ThreadSubscriptionMessage::Operation { operation };
            inner
                .threads
                .get_mut(&thread_id)
                .expect("thread state was synchronized under the same lock")
                .subscribers
                .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        }
        Ok(matches!(
            status,
            atra_protocol::ProcessStatus::Exited { .. }
                | atra_protocol::ProcessStatus::Unavailable { .. }
        ))
    }

    pub(super) async fn delete_thread(&self, thread_id: ThreadId) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if let Some(view) = inner.threads.remove(&thread_id) {
            terminal_thread(view.subscribers, SubscriptionTerminal::Deleted);
        }
        let checkpoints = inner
            .checkpoints
            .iter()
            .filter_map(|(id, view)| (view.state.metadata().thread_id == thread_id).then_some(*id))
            .collect::<Vec<_>>();
        for checkpoint_id in checkpoints {
            let view = inner
                .checkpoints
                .remove(&checkpoint_id)
                .expect("checkpoint was collected from the same map");
            terminal_checkpoint(view.subscribers, SubscriptionTerminal::Deleted);
        }
        let processes = inner
            .processes
            .keys()
            .filter(|process| process.thread_id() == thread_id)
            .cloned()
            .collect::<Vec<_>>();
        for process in processes {
            let view = inner
                .processes
                .remove(&process)
                .expect("process was collected from the same map");
            terminal_process(view.subscribers, SubscriptionTerminal::Deleted);
        }
        if inner
            .controller
            .state
            .threads()
            .iter()
            .any(|thread| thread.id == thread_id)
        {
            let operation = ControllerOperation::ThreadRemoved { thread_id };
            operation
                .clone()
                .apply(&mut inner.controller.state)
                .context("failed to remove thread from controller state")?;
            let message = ControllerSubscriptionMessage::Operation { operation };
            inner
                .controller
                .subscribers
                .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        } else {
            bail!("thread does not exist in controller state");
        }
        Ok(())
    }

    pub(super) async fn shutdown(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if inner_lifecycle(&inner) == atra_protocol::ControllerLifecycle::Stopping {
            bail!("controller is already stopping");
        }
        let operation = ControllerOperation::LifecycleChanged {
            lifecycle: atra_protocol::ControllerLifecycle::Stopping,
        };
        operation
            .clone()
            .apply(&mut inner.controller.state)
            .context("failed to stop controller state")?;
        let message = ControllerSubscriptionMessage::Operation { operation };
        inner
            .controller
            .subscribers
            .retain(|subscriber| subscriber.send(message.clone()).is_ok());
        terminal(
            std::mem::take(&mut inner.controller.subscribers),
            SubscriptionTerminal::ControllerShutdown,
        );
        for (_, view) in inner.threads.drain() {
            terminal_thread(view.subscribers, SubscriptionTerminal::ControllerShutdown);
        }
        for (_, view) in inner.checkpoints.drain() {
            terminal_checkpoint(view.subscribers, SubscriptionTerminal::ControllerShutdown);
        }
        for (_, view) in inner.processes.drain() {
            terminal_process(view.subscribers, SubscriptionTerminal::ControllerShutdown);
        }
        Ok(())
    }
}

fn inner_lifecycle(inner: &ViewStates) -> atra_protocol::ControllerLifecycle {
    inner.controller.state.lifecycle()
}

fn ensure_inner_running(inner: &ViewStates) -> Result<()> {
    if inner_lifecycle(inner) == atra_protocol::ControllerLifecycle::Stopping {
        bail!("controller is stopping");
    }
    Ok(())
}

fn suffix_prefix_overlap(previous: &str, current: &str) -> usize {
    let mut overlap = previous.len().min(current.len());
    while overlap > 0 {
        if previous.is_char_boundary(previous.len() - overlap)
            && current.is_char_boundary(overlap)
            && previous[previous.len() - overlap..] == current[..overlap]
        {
            return overlap;
        }
        overlap -= 1;
    }
    0
}

fn terminal(
    subscribers: Vec<mpsc::UnboundedSender<ControllerSubscriptionMessage>>,
    terminal: SubscriptionTerminal,
) {
    for subscriber in subscribers {
        let _ = subscriber.send(ControllerSubscriptionMessage::Terminal {
            terminal: terminal.clone(),
        });
    }
}

fn terminal_checkpoint(
    subscribers: Vec<mpsc::UnboundedSender<CheckpointSubscriptionMessage>>,
    terminal: SubscriptionTerminal,
) {
    for subscriber in subscribers {
        let _ = subscriber.send(CheckpointSubscriptionMessage::Terminal {
            terminal: terminal.clone(),
        });
    }
}

fn terminal_thread(
    subscribers: Vec<mpsc::UnboundedSender<ThreadSubscriptionMessage>>,
    terminal: SubscriptionTerminal,
) {
    for subscriber in subscribers {
        let _ = subscriber.send(ThreadSubscriptionMessage::Terminal {
            terminal: terminal.clone(),
        });
    }
}

fn terminal_process(
    subscribers: Vec<mpsc::UnboundedSender<ProcessSubscriptionMessage>>,
    terminal: SubscriptionTerminal,
) {
    for subscriber in subscribers {
        let _ = subscriber.send(ProcessSubscriptionMessage::Terminal {
            terminal: terminal.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscription_started_while_stopping_gets_a_terminal() {
        let views = Views::new(ControllerState::new(
            atra_protocol::ControllerLifecycle::Running,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        views.shutdown().await.unwrap();

        let mut subscription = views.subscribe_controller().await;
        let ControllerSubscriptionMessage::Snapshot { state } = subscription.recv().await.unwrap()
        else {
            panic!("subscription did not start with a snapshot");
        };
        assert_eq!(
            state.lifecycle(),
            atra_protocol::ControllerLifecycle::Stopping
        );
        assert_eq!(
            subscription.recv().await.unwrap(),
            ControllerSubscriptionMessage::Terminal {
                terminal: SubscriptionTerminal::ControllerShutdown,
            }
        );
        assert!(subscription.recv().await.is_none());
    }

    #[tokio::test]
    async fn stale_running_inspection_cannot_overwrite_stopped_process() {
        let thread_id = ThreadId(1);
        let views = Views::new(ControllerState::new(
            atra_protocol::ControllerLifecycle::Running,
            vec![atra_protocol::Thread {
                id: thread_id,
                display_name: None,
                provider: "fake".to_owned(),
                model: "test".to_owned(),
                reasoning_effort: "medium".to_owned(),
            }],
            Vec::new(),
            Vec::new(),
        ));
        views
            .insert_thread(
                ThreadState::materialize(
                    atra_protocol::Thread {
                        id: thread_id,
                        display_name: None,
                        provider: "fake".to_owned(),
                        model: "test".to_owned(),
                        reasoning_effort: "medium".to_owned(),
                    },
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await;
        let locator = ProcessLocator::new(
            thread_id,
            "local".to_owned(),
            atra_protocol::ProcessId("process-1".to_owned()),
        );
        views
            .insert_process(ProcessState::new(
                atra_protocol::ProcessSummary::new(
                    locator.clone(),
                    "echo done".to_owned(),
                    1,
                    atra_protocol::ProcessStatus::Running,
                ),
                "partial".to_owned(),
                0,
            ))
            .await
            .unwrap();

        views
            .synchronize_process(
                &locator,
                "final".to_owned(),
                0,
                atra_protocol::ProcessStatus::Exited { exit_code: None },
            )
            .await
            .unwrap();
        assert!(
            views
                .synchronize_process(
                    &locator,
                    "stale".to_owned(),
                    0,
                    atra_protocol::ProcessStatus::Running,
                )
                .await
                .is_err()
        );

        let mut subscription = views.subscribe_process(&locator).await.unwrap();
        let ProcessSubscriptionMessage::Snapshot { state } = subscription.recv().await.unwrap()
        else {
            panic!("subscription did not start with a snapshot");
        };
        assert_eq!(state.output_tail(), "final");
        assert_eq!(
            state.process().status(),
            &atra_protocol::ProcessStatus::Exited { exit_code: None }
        );
    }
}
