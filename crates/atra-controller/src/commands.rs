use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::{
    State, checkpoint_time_ms,
    model::ModelStreamEvent,
    protocol_events, provider_state,
    turn::{TurnCompletion, TurnRequest},
};
use anyhow::{Context, Result, bail};
use atra_protocol::{
    ActiveItem, ActiveItemData, ActiveItemId, Command, CommandResult, ControllerOperation,
    ProcessStatus, ProviderLifecycle, ProviderState, ThreadEvent, ThreadEventData, ThreadOperation,
    ToolCallEvent, ToolResultEvent, TurnOutcome, TurnPhase,
};

impl State {
    pub(super) async fn handle_command(
        self: &Arc<Self>,
        command: Command,
    ) -> Result<CommandResult> {
        self.views.ensure_running().await?;
        match command {
            Command::Shutdown => bail!("shutdown must be handled by the connection owner"),
            Command::ThreadCreate { display_name } => {
                let _mutation = self.lock_mutation().await?;
                let thread_id = self
                    .store
                    .create_thread(
                        display_name,
                        self.default_provider.clone(),
                        crate::model::DEFAULT_MODEL.to_owned(),
                        "medium".to_owned(),
                    )
                    .await?;
                let thread = self
                    .store
                    .thread(thread_id)
                    .await
                    .context("failed to load created thread")?;
                self.views.add_thread(thread).await?;
                Ok(CommandResult::ThreadCreated { thread_id })
            }
            Command::ThreadRename {
                thread_id,
                display_name,
            } => {
                self.materialize_thread(thread_id).await?;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot rename a thread while a turn is active");
                }
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot rename a thread while a turn is active");
                }
                let _mutation = self.lock_mutation().await?;
                if display_name.trim().is_empty() {
                    bail!("thread display name must not be empty");
                }
                self.store.rename_thread(thread_id, display_name).await?;
                let metadata = self.store.thread(thread_id).await?;
                self.views
                    .update_thread_metadata(thread_id, metadata)
                    .await?;
                Ok(CommandResult::Accepted)
            }
            Command::ThreadDelete { thread_id } => {
                self.materialize_thread(thread_id).await?;
                self.turns.begin_delete(thread_id).await?;
                let result = async {
                    let _guard = self.thread_lock(thread_id).lock_owned().await;
                    self.runners.stop_thread_processes(thread_id).await;
                    let _mutation = self.lock_mutation().await?;
                    self.store.delete_thread(thread_id).await?;
                    self.views.delete_thread(thread_id).await
                }
                .await;
                self.turns.finish_delete(thread_id).await;
                result?;
                Ok(CommandResult::Accepted)
            }
            Command::ThreadSetModel {
                thread_id,
                provider,
                model,
                reasoning_effort,
            } => {
                self.materialize_thread(thread_id).await?;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot change the model while a turn is active");
                }
                if model.trim().is_empty() {
                    bail!("thread model must not be empty");
                }
                if reasoning_effort.trim().is_empty() {
                    bail!("reasoning effort must not be empty");
                }
                let models = self.provider(&provider)?.models().await?;
                let selected = models
                    .iter()
                    .find(|candidate| candidate.provider == provider && candidate.id == model)
                    .with_context(|| format!("unknown model {provider}/{model}"))?;
                if !selected
                    .supported_reasoning_efforts
                    .iter()
                    .any(|candidate| candidate == &reasoning_effort)
                {
                    bail!(
                        "reasoning effort {reasoning_effort} is not supported by model {provider}/{model}"
                    );
                }
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot change the model while a turn is active");
                }
                let _mutation = self.lock_mutation().await?;
                let (current_provider, _, _) = self.store.thread_model(thread_id).await?;
                if current_provider != provider && !self.store.events(thread_id).await?.is_empty() {
                    bail!("cannot change provider after the thread history has started");
                }
                self.store
                    .set_thread_model(thread_id, provider, model, reasoning_effort)
                    .await?;
                let metadata = self.store.thread(thread_id).await?;
                self.views
                    .update_thread_metadata(thread_id, metadata)
                    .await?;
                Ok(CommandResult::Accepted)
            }
            Command::ThreadCheckpointCreate { thread_id } => {
                self.materialize_thread(thread_id).await?;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot create a checkpoint while a turn is active");
                }
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot create a checkpoint while a turn is active");
                }
                let _mutation = self.lock_mutation().await?;
                self.ensure_no_pending_approval(thread_id).await?;
                let checkpoint_id = self
                    .store
                    .create_checkpoint(thread_id, checkpoint_time_ms(), "manual".to_owned())
                    .await
                    .context("failed to create checkpoint")?;
                let checkpoint = self.store.checkpoint(checkpoint_id).await?;
                self.views
                    .apply_thread(thread_id, ThreadOperation::CheckpointAdded { checkpoint })
                    .await?;
                Ok(CommandResult::Accepted)
            }
            Command::ThreadFork {
                thread_id,
                checkpoint_id,
                sequence,
                display_name,
            } => {
                self.materialize_thread(thread_id).await?;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot fork a thread while a turn is active");
                }
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot fork a thread while a turn is active");
                }
                let _mutation = self.lock_mutation().await?;
                self.ensure_no_pending_approval(thread_id).await?;
                let forked_id = self
                    .store
                    .fork_thread(thread_id, checkpoint_id, sequence, display_name)
                    .await
                    .context("failed to fork thread")?;
                let thread = self.store.thread(forked_id).await?;
                self.views.add_thread(thread).await?;
                Ok(CommandResult::ThreadForked {
                    thread_id: forked_id,
                })
            }
            Command::ThreadReplaceHistory { thread_id, target } => {
                self.materialize_thread(thread_id).await?;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot replace history while a turn is active");
                }
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                if self.turns.get(thread_id).await.is_some() {
                    bail!("cannot replace history while a turn is active");
                }
                let _mutation = self.lock_mutation().await?;
                self.ensure_no_pending_approval(thread_id).await?;
                let checkpoint_id = self
                    .store
                    .replace_history(thread_id, target, checkpoint_time_ms())
                    .await
                    .context("failed to replace thread history")?;
                let metadata = self.store.thread(thread_id).await?;
                let events = protocol_events(self.store.events(thread_id).await?);
                let checkpoint = self.store.checkpoint(checkpoint_id).await?;
                self.views
                    .replace_thread_history(thread_id, metadata, events, checkpoint)
                    .await?;
                Ok(CommandResult::Accepted)
            }
            Command::ThreadCancel { thread_id } => {
                self.materialize_thread(thread_id).await?;
                self.turns
                    .get(thread_id)
                    .await
                    .context("thread has no active turn")?;
                self.views.start_cancellation(thread_id).await?;
                let state = Arc::clone(self);
                tokio::spawn(async move {
                    if let Err(error) = state.cancel_thread(thread_id).await {
                        tracing::error!(thread_id = %thread_id, error = %format!("{error:#}"), "turn cancellation failed");
                    }
                });
                Ok(CommandResult::Accepted)
            }
            Command::ApprovalAllow { approval_id } => {
                let approval = self.claim_approval(approval_id).await?;
                self.views.resolve_approval(approval_id).await?;
                approval.resolve(approval_id, true, None)?;
                Ok(CommandResult::Accepted)
            }
            Command::ApprovalDeny {
                approval_id,
                reason,
            } => {
                let approval = self.claim_approval(approval_id).await?;
                self.views.resolve_approval(approval_id).await?;
                approval.resolve(approval_id, false, reason)?;
                Ok(CommandResult::Accepted)
            }
            Command::ThreadSend { thread_id, message } => {
                self.start_turn(
                    thread_id,
                    TurnPhase::Running,
                    TurnRequest::Send { thread_id, message },
                )
                .await
            }
            Command::ThreadContinue { thread_id } => {
                self.start_turn(
                    thread_id,
                    TurnPhase::Running,
                    TurnRequest::Continue { thread_id },
                )
                .await
            }
            Command::ThreadCompact { thread_id } => {
                self.start_turn(
                    thread_id,
                    TurnPhase::Compacting,
                    TurnRequest::Compact { thread_id },
                )
                .await
            }
            Command::ProviderLogin {
                provider,
                credential,
            } => {
                self.start_provider_operation(
                    provider,
                    ProviderLifecycle::LoggingIn,
                    ProviderTask::Login(credential),
                )
                .await
            }
            Command::ProviderReloadAuth { provider } => {
                self.start_provider_operation(
                    provider,
                    ProviderLifecycle::Refreshing,
                    ProviderTask::Reload,
                )
                .await
            }
            Command::ProviderLogout { provider } => {
                self.start_provider_operation(
                    provider,
                    ProviderLifecycle::LoggingOut,
                    ProviderTask::Logout,
                )
                .await
            }
            Command::RunnerLaunch {
                name,
                description,
                approval,
                command,
            } => {
                let runner = atra_protocol::Runner {
                    name: name.clone(),
                    description: description.clone(),
                };
                self.views.start_runner_launch(runner.clone()).await?;
                let state = Arc::clone(self);
                tokio::spawn(async move {
                    let result = state
                        .launch_runner(name.clone(), description, approval, command)
                        .await;
                    let launched = result.as_ref().copied().unwrap_or(false);
                    let lifecycle = match result {
                        Ok(_) => atra_protocol::RunnerLifecycle::Running,
                        Err(error) => atra_protocol::RunnerLifecycle::Failed {
                            message: format!("{error:#}"),
                        },
                    };
                    if let Err(error) = state
                        .views
                        .apply_controller(ControllerOperation::RunnerUpdated {
                            runner: atra_protocol::RunnerState::new(runner.clone(), lifecycle),
                        })
                        .await
                    {
                        tracing::error!(runner = name, error = %format!("{error:#}"), "failed to update public Runner state");
                    }
                    if launched {
                        let state = Arc::downgrade(&state);
                        watch_runner(state, name, runner).await;
                    }
                });
                Ok(CommandResult::Accepted)
            }
            Command::ExecCommand {
                thread_id,
                runner,
                command,
            } => {
                self.materialize_thread(thread_id).await?;
                let _guard = self.thread_lock(thread_id).lock_owned().await;
                {
                    let _mutation = self.lock_mutation().await?;
                    self.store
                        .thread(thread_id)
                        .await
                        .context("thread no longer exists")?;
                }
                let running_runner = self.runners.get(&runner).await?;
                let process_id = self
                    .start_managed_process(thread_id, &runner, &running_runner, command)
                    .await?;
                Ok(CommandResult::ProcessStarted { process_id })
            }
            Command::StopProcess { process } => {
                self.materialize_thread(process.thread_id()).await?;
                self.materialize_process(&process).await?;
                let key = crate::ProcessKey {
                    thread_id: process.thread_id(),
                    runner: process.runner().to_owned(),
                    process_id: process.process_id().clone(),
                };
                let output = self.runners.stop_process(&key).await?;
                self.views
                    .synchronize_process(
                        &process,
                        output.content,
                        output.omitted_bytes,
                        ProcessStatus::Exited { exit_code: None },
                    )
                    .await?;
                Ok(CommandResult::Accepted)
            }
        }
    }

    async fn start_turn(
        self: &Arc<Self>,
        thread_id: atra_protocol::ThreadId,
        phase: TurnPhase,
        request: TurnRequest,
    ) -> Result<CommandResult> {
        self.materialize_thread(thread_id).await?;
        let active = self.turns.start(thread_id).await?;
        if let Err(error) = self
            .views
            .apply_thread(thread_id, ThreadOperation::ActiveTurnStarted { phase })
            .await
        {
            self.turns.finish(thread_id, &active).await;
            return Err(error);
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let lifecycle_active = Arc::clone(&active);
            let updates = TurnProjector::new(&state, thread_id);
            let response = state
                .handle_started_streaming(request, active, &updates)
                .await;
            let outcome = match response {
                Ok(TurnCompletion::Cancelled) => TurnOutcome::Cancelled,
                Ok(TurnCompletion::Completed | TurnCompletion::Compacted) => TurnOutcome::Completed,
                Err(error) => TurnOutcome::Failed {
                    message: format!("{error:#}"),
                },
            };
            if let Err(error) = state
                .views
                .apply_thread(thread_id, ThreadOperation::TurnFinished { outcome })
                .await
            {
                tracing::error!(thread_id = %thread_id, error = %format!("{error:#}"), "failed to finish public turn state");
            }
            state.turns.finish(thread_id, &lifecycle_active).await;
            if let Ok((provider_id, _, _)) = state.store.thread_model(thread_id).await
                && let Ok(provider) = state.provider(&provider_id)
            {
                let provider = provider_state(&provider_id, provider).await;
                if let Err(error) = state
                    .views
                    .apply_controller(ControllerOperation::ProviderUpdated { provider })
                    .await
                {
                    tracing::error!(provider = provider_id, error = %format!("{error:#}"), "failed to refresh public provider state");
                }
            }
        });
        Ok(CommandResult::Accepted)
    }

    async fn start_provider_operation(
        self: &Arc<Self>,
        provider_id: String,
        lifecycle: ProviderLifecycle,
        task: ProviderTask,
    ) -> Result<CommandResult> {
        let provider = Arc::clone(self.provider(&provider_id)?);
        self.views
            .start_provider_operation(&provider_id, lifecycle)
            .await?;
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let result = match task {
                ProviderTask::Login(credential) => provider.login(credential).await.map(|_| ()),
                ProviderTask::Reload => provider.reload_auth().await,
                ProviderTask::Logout => provider.logout().await,
            };
            let public = match result {
                Ok(()) => provider_state(&provider_id, &provider).await,
                Err(error) => ProviderState::new(
                    provider_id.clone(),
                    ProviderLifecycle::Failed {
                        message: format!("{error:#}"),
                    },
                    Vec::new(),
                    None,
                ),
            };
            if let Err(error) = state
                .views
                .apply_controller(ControllerOperation::ProviderUpdated { provider: public })
                .await
            {
                tracing::error!(provider = provider_id, error = %format!("{error:#}"), "failed to update public provider state");
            }
        });
        Ok(CommandResult::Accepted)
    }
}

enum ProviderTask {
    Login(Option<String>),
    Reload,
    Logout,
}

async fn watch_runner(state: std::sync::Weak<State>, name: String, runner: atra_protocol::Runner) {
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Some(state) = state.upgrade() else {
            return;
        };
        let failure = match state.runners.list().await {
            Ok(runners) if runners.iter().any(|current| current.name == name) => continue,
            Ok(_) => "Runner exited".to_owned(),
            Err(error) => format!("failed to inspect Runner: {error:#}"),
        };
        if let Err(error) = state
            .views
            .apply_controller(ControllerOperation::RunnerUpdated {
                runner: atra_protocol::RunnerState::new(
                    runner,
                    atra_protocol::RunnerLifecycle::Failed { message: failure },
                ),
            })
            .await
        {
            tracing::error!(runner = name, error = %format!("{error:#}"), "failed to update stopped Runner state");
        }
        return;
    }
}

pub(super) struct TurnProjector {
    state: Arc<State>,
    thread_id: atra_protocol::ThreadId,
    projection: tokio::sync::Mutex<TurnProjection>,
}

impl TurnProjector {
    fn new(state: &Arc<State>, thread_id: atra_protocol::ThreadId) -> Self {
        Self {
            state: Arc::clone(state),
            thread_id,
            projection: tokio::sync::Mutex::new(TurnProjection::new()),
        }
    }

    pub(super) async fn apply_update(&self, update: ModelStreamEvent) -> Result<()> {
        let mut projection = self.projection.lock().await;
        let operation = match update {
            ModelStreamEvent::Retry { current, max } => {
                tracing::debug!(current, max, "model request retrying");
                projection.retrying = true;
                ThreadOperation::PhaseChanged {
                    phase: TurnPhase::Retrying,
                }
            }
            ModelStreamEvent::AssistantDelta(content) => {
                restore_running_phase(&self.state, self.thread_id, &mut projection).await?;
                match projection.assistant {
                    Some(id) => ThreadOperation::ActiveTextAppended { id, content },
                    None => {
                        let id = projection.id();
                        projection.assistant = Some(id);
                        ThreadOperation::ActiveItemAdded {
                            item: ActiveItem::new(id, ActiveItemData::Assistant { content }),
                        }
                    }
                }
            }
            ModelStreamEvent::ReasoningSummaryDelta(content) => {
                restore_running_phase(&self.state, self.thread_id, &mut projection).await?;
                match projection.reasoning {
                    Some(id) => ThreadOperation::ActiveTextAppended { id, content },
                    None => {
                        let id = projection.id();
                        projection.reasoning = Some(id);
                        ThreadOperation::ActiveItemAdded {
                            item: ActiveItem::new(id, ActiveItemData::Reasoning { content }),
                        }
                    }
                }
            }
            ModelStreamEvent::WebSearchUpdate { item_id, action } => {
                restore_running_phase(&self.state, self.thread_id, &mut projection).await?;
                match projection.web_searches.get(&item_id).copied() {
                    Some(id) => ThreadOperation::ActiveWebSearchUpdated { id, action },
                    None => {
                        let id = projection.id();
                        projection.web_searches.insert(item_id.clone(), id);
                        ThreadOperation::ActiveItemAdded {
                            item: ActiveItem::new(
                                id,
                                ActiveItemData::WebSearch { item_id, action },
                            ),
                        }
                    }
                }
            }
            ModelStreamEvent::ToolCallStarted {
                item_id,
                call_id,
                name,
            } => {
                restore_running_phase(&self.state, self.thread_id, &mut projection).await?;
                let id = projection.id();
                projection.tool_calls.insert(item_id.clone(), id);
                if let Some(call_id) = call_id {
                    projection.tool_calls.insert(call_id, id);
                }
                ThreadOperation::ActiveItemAdded {
                    item: ActiveItem::new(
                        id,
                        ActiveItemData::ToolCall {
                            item_id,
                            name,
                            input: String::new(),
                        },
                    ),
                }
            }
            ModelStreamEvent::ToolCallDelta { item_id, delta } => {
                let id = projection
                    .tool_calls
                    .get(&item_id)
                    .copied()
                    .context("tool call delta arrived before tool call start")?;
                ThreadOperation::ActiveTextAppended { id, content: delta }
            }
            ModelStreamEvent::RunnerOperationUpdate {
                call_id,
                operation_index,
                update,
            } => {
                let key = (call_id.clone(), operation_index);
                match projection.runner_tools.get(&key).copied() {
                    Some(id) => ThreadOperation::ActiveRunnerUpdated { id, update },
                    None => {
                        let id = projection.id();
                        projection.runner_tools.insert(key, id);
                        ThreadOperation::ActiveItemAdded {
                            item: ActiveItem::new(
                                id,
                                ActiveItemData::RunnerTool {
                                    call_id,
                                    operation_index,
                                    update,
                                },
                            ),
                        }
                    }
                }
            }
        };
        self.state
            .views
            .apply_thread(self.thread_id, operation)
            .await
    }

    pub(super) async fn approval_requested(
        &self,
        approval: atra_protocol::PendingApproval,
    ) -> Result<()> {
        self.state
            .views
            .apply_thread(
                self.thread_id,
                ThreadOperation::ApprovalRequested { approval },
            )
            .await
    }

    pub(super) async fn event_finalized(&self, event: ThreadEvent) -> Result<()> {
        let mut projection = self.projection.lock().await;
        project_final_event(&self.state, self.thread_id, &mut projection, event).await
    }

    pub(super) async fn history_replaced(&self, events: Vec<ThreadEvent>) -> Result<()> {
        self.state
            .views
            .apply_thread(self.thread_id, ThreadOperation::EventsReplaced { events })
            .await
    }

    pub(super) async fn checkpoint_added(
        &self,
        checkpoint: atra_protocol::ThreadCheckpoint,
    ) -> Result<()> {
        self.state
            .views
            .apply_thread(
                self.thread_id,
                ThreadOperation::CheckpointAdded { checkpoint },
            )
            .await
    }
}

struct TurnProjection {
    next_id: u64,
    assistant: Option<ActiveItemId>,
    reasoning: Option<ActiveItemId>,
    web_searches: HashMap<String, ActiveItemId>,
    tool_calls: HashMap<String, ActiveItemId>,
    runner_tools: HashMap<(String, usize), ActiveItemId>,
    retrying: bool,
}

impl TurnProjection {
    fn new() -> Self {
        Self {
            next_id: 0,
            assistant: None,
            reasoning: None,
            web_searches: HashMap::new(),
            tool_calls: HashMap::new(),
            runner_tools: HashMap::new(),
            retrying: false,
        }
    }

    fn id(&mut self) -> ActiveItemId {
        self.next_id += 1;
        ActiveItemId(self.next_id)
    }

    fn finish_tool_call(&mut self, key: Option<&str>) -> Option<ActiveItemId> {
        let active_id = key.and_then(|key| self.tool_calls.remove(key))?;
        self.tool_calls.retain(|_, current| *current != active_id);
        Some(active_id)
    }
}

async fn restore_running_phase(
    state: &State,
    thread_id: atra_protocol::ThreadId,
    projection: &mut TurnProjection,
) -> Result<()> {
    if projection.retrying {
        state
            .views
            .apply_thread(
                thread_id,
                ThreadOperation::PhaseChanged {
                    phase: TurnPhase::Running,
                },
            )
            .await?;
        projection.retrying = false;
    }
    Ok(())
}

async fn project_final_event(
    state: &State,
    thread_id: atra_protocol::ThreadId,
    projection: &mut TurnProjection,
    event: ThreadEvent,
) -> Result<()> {
    let active_id = match &event.data {
        ThreadEventData::AssistantMessage(_) => projection.assistant.take(),
        ThreadEventData::Reasoning(_) => projection.reasoning.take(),
        ThreadEventData::WebSearch(item) => item
            .item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| projection.web_searches.remove(id))
            .or_else(|| take_first(&mut projection.web_searches)),
        ThreadEventData::ToolCall(call) => projection.finish_tool_call(tool_call_key(call)),
        ThreadEventData::ToolResult(result) => {
            if let Some(call_id) = tool_result_call_id(result) {
                let ids = projection
                    .runner_tools
                    .iter()
                    .filter_map(|((current, _), id)| (current == call_id).then_some(*id))
                    .collect::<Vec<_>>();
                projection
                    .runner_tools
                    .retain(|(current, _), _| current != call_id);
                for id in ids {
                    state
                        .views
                        .apply_thread(thread_id, ThreadOperation::ActiveItemDiscarded { id })
                        .await?;
                }
            }
            None
        }
        _ => None,
    };
    let operation = match active_id {
        Some(active_id) => ThreadOperation::ActiveItemFinalized { active_id, event },
        None => ThreadOperation::EventAppended { event },
    };
    state.views.apply_thread(thread_id, operation).await?;
    Ok(())
}

fn take_first(values: &mut HashMap<String, ActiveItemId>) -> Option<ActiveItemId> {
    let key = values.keys().next()?.clone();
    values.remove(&key)
}

fn tool_call_key(call: &ToolCallEvent) -> Option<&str> {
    match call {
        ToolCallEvent::Custom { item_id, .. } => item_id.as_deref(),
        ToolCallEvent::Function { call_id, .. } => call_id.as_deref(),
    }
}

fn tool_result_call_id(result: &ToolResultEvent) -> Option<&str> {
    match result {
        ToolResultEvent::Custom { call_id, .. } | ToolResultEvent::Function { call_id, .. } => {
            call_id.as_deref()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_call_finalization_uses_the_explicit_call_id() {
        let mut projection = TurnProjection::new();
        let first = projection.id();
        projection.tool_calls.insert("item-1".to_owned(), first);
        projection.tool_calls.insert("call-1".to_owned(), first);
        let second = projection.id();
        projection.tool_calls.insert("item-2".to_owned(), second);
        projection.tool_calls.insert("call-2".to_owned(), second);

        assert_eq!(projection.finish_tool_call(Some("call-2")), Some(second));
        assert_eq!(projection.finish_tool_call(Some("missing")), None);
        assert_eq!(projection.finish_tool_call(Some("item-1")), Some(first));
        assert!(projection.tool_calls.is_empty());
    }
}
