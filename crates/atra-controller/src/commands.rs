use std::{any::Any, collections::HashMap, future::Future, panic::AssertUnwindSafe, sync::Arc};

use crate::{
    State, checkpoint_time_ms,
    model::ModelStreamEvent,
    protocol_events, provider_state,
    turn::{TurnCompletion, TurnRequest},
};
use anyhow::{Context, Result, bail};
use atra_protocol::{
    ActiveItem, ActiveItemData, ActiveItemId, Command, CommandResult, ControllerOperation,
    EventSequence, ProviderLifecycle, ProviderState, RetryStatus, ThreadEvent, ThreadEventData,
    ThreadOperation, ToolCallEvent, ToolResultEvent, TurnOutcome, TurnPhase,
};
use futures_util::FutureExt;

impl State {
    pub(super) async fn handle_command(
        self: &Arc<Self>,
        command: Command,
    ) -> Result<CommandResult> {
        self.views.ensure_running().await?;
        match command {
            Command::Shutdown => bail!("shutdown must be handled by the connection owner"),
            Command::SkillList => {
                let generation = self.collect_skill_generation().await?;
                Ok(CommandResult::SkillsListed {
                    skills: generation
                        .skills
                        .iter()
                        .map(|skill| skill.name.clone())
                        .collect(),
                })
            }
            Command::ThreadCreate { display_name } => {
                let _mutation = self.lock_mutation().await?;
                let thread_id = self
                    .store
                    .create_thread(
                        display_name,
                        self.providers.default_provider().to_owned(),
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
                let _mutation = self.lock_mutation().await?;
                self.turns.ensure_mutable(thread_id)?;
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
                self.delete_thread_subtree(thread_id, false).await?;
                Ok(CommandResult::Accepted)
            }
            Command::ThreadDeleteRecursive { thread_id } => {
                self.materialize_thread(thread_id).await?;
                self.delete_thread_subtree(thread_id, true).await?;
                Ok(CommandResult::Accepted)
            }
            Command::ThreadSetModel {
                thread_id,
                provider,
                model,
                reasoning_effort,
            } => {
                self.materialize_thread(thread_id).await?;
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
                let _mutation = self.lock_mutation().await?;
                self.turns.ensure_mutable(thread_id)?;
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
                let _mutation = self.lock_mutation().await?;
                self.turns.ensure_mutable(thread_id)?;
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
                let _mutation = self.lock_mutation().await?;
                self.turns.ensure_mutable(thread_id)?;
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
                let _mutation = self.lock_mutation().await?;
                self.turns.ensure_mutable(thread_id)?;
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
                    .begin_cancellation(thread_id)
                    .context("thread has no active turn")?;
                Ok(CommandResult::Accepted)
            }
            Command::ApprovalAllow { approval_id } => {
                let (thread_id, approval) = self.claim_approval(approval_id).await?;
                persist_public_event(
                    self,
                    thread_id,
                    ThreadEventData::ApprovalDecision(atra_protocol::ApprovalDecisionEvent {
                        interaction_id: approval_id,
                        allowed: true,
                        reason: None,
                    }),
                )
                .await?;
                self.views
                    .resolve_interaction(thread_id, approval_id)
                    .await?;
                approval.resolve_approval(approval_id, true, None)?;
                Ok(CommandResult::Accepted)
            }
            Command::ApprovalDeny {
                approval_id,
                reason,
            } => {
                let (thread_id, approval) = self.claim_approval(approval_id).await?;
                persist_public_event(
                    self,
                    thread_id,
                    ThreadEventData::ApprovalDecision(atra_protocol::ApprovalDecisionEvent {
                        interaction_id: approval_id,
                        allowed: false,
                        reason: reason.clone(),
                    }),
                )
                .await?;
                self.views
                    .resolve_interaction(thread_id, approval_id)
                    .await?;
                approval.resolve_approval(approval_id, false, reason)?;
                Ok(CommandResult::Accepted)
            }
            Command::QuestionAnswer {
                request_id,
                answers,
            } => {
                self.views
                    .validate_question_answers(request_id, &answers)
                    .await?;
                let (thread_id, question) = self.claim_questions(request_id).await?;
                self.views
                    .resolve_interaction(thread_id, request_id)
                    .await?;
                question.resolve_questions(request_id, answers)?;
                Ok(CommandResult::Accepted)
            }
            Command::ThreadSend {
                thread_id,
                message,
                allow_questions,
            } => {
                self.start_turn(
                    thread_id,
                    TurnPhase::Running,
                    TurnRequest::Send {
                        thread_id,
                        message,
                        allow_questions,
                    },
                )
                .await
            }
            Command::ThreadContinue {
                thread_id,
                allow_questions,
            } => {
                self.start_turn(
                    thread_id,
                    TurnPhase::Running,
                    TurnRequest::Continue {
                        thread_id,
                        allow_questions,
                    },
                )
                .await
            }
            Command::ThreadCompact {
                thread_id,
                allow_questions,
            } => {
                self.start_turn(
                    thread_id,
                    TurnPhase::Compacting,
                    TurnRequest::Compact {
                        thread_id,
                        allow_questions,
                    },
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
                self.launch_runner(name, description, approval, command)
                    .await?;
                Ok(CommandResult::Accepted)
            }
            Command::ExecCommand {
                thread_id,
                runner,
                command,
            } => {
                self.materialize_thread(thread_id).await?;
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
                self.runners.stop_process(&key).await?;
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
        let active = {
            let _mutation = self.lock_mutation().await?;
            self.materialize_thread_locked(thread_id).await?;
            self.turns.start(thread_id)?
        };
        self.launch_turn(thread_id, phase, request, active).await
    }

    pub(super) async fn start_agent_turn(
        self: &Arc<Self>,
        invoking: atra_protocol::ThreadId,
        thread_id: atra_protocol::ThreadId,
        message: String,
    ) -> Result<(CommandResult, EventSequence)> {
        let state = Arc::clone(self);
        crate::agent::run_callback_operation("agent send", async move {
            let mutation = state.lock_mutation().await?;
            if thread_id == invoking || !state.store.is_descendant(invoking, thread_id).await? {
                bail!("thread {thread_id} is not a descendant of invoking thread {invoking}");
            }
            state.materialize_thread_locked(thread_id).await?;
            let root = state.store.root_thread(invoking).await?;
            let after = state
                .store
                .events(thread_id)
                .await?
                .last()
                .map_or(EventSequence(-1), |event| event.sequence);
            let active = state.turns.start_agent(thread_id, root)?;
            Ok(async move {
                let _mutation = mutation;
                if let Err(error) = state
                    .views
                    .apply_thread(
                        thread_id,
                        ThreadOperation::ActiveTurnStarted {
                            phase: TurnPhase::Running,
                        },
                    )
                    .await
                {
                    state.turns.finish(thread_id);
                    return Err(error);
                }
                let result = state.launch_registered_turn(
                    thread_id,
                    TurnRequest::Send {
                        thread_id,
                        message,
                        allow_questions: false,
                    },
                    active,
                );
                Ok((result, after))
            })
        })
        .await?
    }

    async fn launch_turn(
        self: &Arc<Self>,
        thread_id: atra_protocol::ThreadId,
        phase: TurnPhase,
        request: TurnRequest,
        active: Arc<crate::lifecycle::ActiveTurn>,
    ) -> Result<CommandResult> {
        if let Err(error) = self
            .views
            .apply_thread(thread_id, ThreadOperation::ActiveTurnStarted { phase })
            .await
        {
            self.turns.finish(thread_id);
            return Err(error);
        }
        Ok(self.launch_registered_turn(thread_id, request, active))
    }

    fn launch_registered_turn(
        self: &Arc<Self>,
        thread_id: atra_protocol::ThreadId,
        request: TurnRequest,
        active: Arc<crate::lifecycle::ActiveTurn>,
    ) -> CommandResult {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let updates = TurnProjector::new(&state, thread_id);
            let publish_state = Arc::clone(&state);
            let finish_state = Arc::clone(&state);
            complete_registered_turn(
                thread_id,
                state.handle_started_streaming(request, active, &updates),
                move |outcome| async move {
                    persist_public_event(
                        &publish_state,
                        thread_id,
                        ThreadEventData::TurnOutcome(outcome.clone()),
                    )
                    .await?;
                    publish_state
                        .views
                        .apply_thread(thread_id, ThreadOperation::TurnFinished { outcome })
                        .await
                },
                move || async move {
                    finish_state.turns.finish(thread_id);
                },
            )
            .await;
            if let Ok((provider_id, _, _)) = state.store.thread_model(thread_id).await
                && let Ok(provider) = state.provider(&provider_id)
            {
                let provider = provider_state(provider).await;
                if let Err(error) = state
                    .views
                    .apply_controller(ControllerOperation::ProviderUpdated { provider })
                    .await
                {
                    tracing::error!(provider = provider_id, error = %format!("{error:#}"), "failed to refresh public provider state");
                }
            }
        });
        CommandResult::Accepted
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
                Ok(()) => provider_state(&provider).await,
                Err(error) => ProviderState::new(
                    provider_id.clone(),
                    provider.auth_method(),
                    provider.credential_source(),
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

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_owned())
}

async fn turn_outcome(future: impl Future<Output = Result<TurnCompletion>>) -> TurnOutcome {
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(Ok(TurnCompletion::Cancelled)) => TurnOutcome::Cancelled,
        Ok(Ok(TurnCompletion::Completed | TurnCompletion::Compacted)) => TurnOutcome::Completed,
        Ok(Err(error)) => TurnOutcome::Failed {
            message: format!("{error:#}"),
        },
        Err(payload) => TurnOutcome::Failed {
            message: format!("turn task panicked: {}", panic_message(payload)),
        },
    }
}

async fn complete_registered_turn<Execution, Publish, Published, Finish, Finished>(
    thread_id: atra_protocol::ThreadId,
    execution: Execution,
    publish: Publish,
    finish: Finish,
) where
    Execution: Future<Output = Result<TurnCompletion>>,
    Publish: FnOnce(TurnOutcome) -> Published,
    Published: Future<Output = Result<()>>,
    Finish: FnOnce() -> Finished,
    Finished: Future<Output = ()>,
{
    let outcome = turn_outcome(execution).await;
    match AssertUnwindSafe(publish(outcome)).catch_unwind().await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::error!(thread_id = %thread_id, error = %format!("{error:#}"), "failed to finish public turn state");
        }
        Err(payload) => {
            tracing::error!(thread_id = %thread_id, panic = %panic_message(payload), "public turn finalization panicked");
        }
    }
    finish().await;
}

enum ProviderTask {
    Login(Option<String>),
    Reload,
    Logout,
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
            ModelStreamEvent::Retry {
                summary,
                current,
                max,
            } => {
                tracing::debug!(current, max, "model request retrying");
                persist_public_event(
                    &self.state,
                    self.thread_id,
                    ThreadEventData::Retry(atra_protocol::RetryEvent {
                        summary: summary.clone(),
                        current,
                        max,
                    }),
                )
                .await?;
                projection.retrying = true;
                ThreadOperation::RetryScheduled {
                    retry: RetryStatus::new(summary, current, max),
                }
            }
            ModelStreamEvent::AssistantDelta { content, phase } => {
                restore_running_phase(&self.state, self.thread_id, &mut projection).await?;
                match projection.assistant {
                    Some(id) => ThreadOperation::ActiveAssistantAppended { id, content, phase },
                    None => {
                        let id = projection.id();
                        projection.assistant = Some(id);
                        ThreadOperation::ActiveItemAdded {
                            item: ActiveItem::new(id, ActiveItemData::Assistant { content, phase }),
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
            ModelStreamEvent::ReasoningSummaryPartAdded => {
                let Some(operation) = projection.reasoning_part_added() else {
                    return Ok(());
                };
                operation
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
                if let Some(call_id) = &call_id {
                    projection.tool_calls.insert(call_id.clone(), id);
                }
                ThreadOperation::ActiveItemAdded {
                    item: ActiveItem::new(
                        id,
                        ActiveItemData::ToolCall {
                            item_id,
                            call_id,
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
                runner,
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
                                    runner,
                                    update,
                                },
                            ),
                        }
                    }
                }
            }
            ModelStreamEvent::RunnerOperationOutput {
                call_id,
                operation_index,
                content,
                omitted_bytes,
                timer,
            } => {
                let id = projection
                    .runner_tools
                    .get(&(call_id, operation_index))
                    .copied()
                    .context("Runner output arrived before the operation started")?;
                ThreadOperation::ActiveRunnerOutputAppended {
                    id,
                    content,
                    omitted_bytes,
                    timer,
                }
            }
        };
        self.state
            .views
            .apply_thread(self.thread_id, operation)
            .await
    }

    pub(super) async fn interaction_requested(
        &self,
        interaction: atra_protocol::PendingInteraction,
    ) -> Result<()> {
        self.state
            .views
            .apply_thread(
                self.thread_id,
                ThreadOperation::InteractionRequested { interaction },
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

async fn persist_public_event(
    state: &State,
    thread_id: atra_protocol::ThreadId,
    data: ThreadEventData,
) -> Result<()> {
    let _mutation = state.lock_mutation().await?;
    let sequence = state.store.append(thread_id, data.clone()).await?;
    state
        .views
        .apply_thread(
            thread_id,
            ThreadOperation::EventAppended {
                event: ThreadEvent { sequence, data },
            },
        )
        .await
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

    fn reasoning_part_added(&self) -> Option<ThreadOperation> {
        Some(ThreadOperation::ActiveTextAppended {
            id: self.reasoning?,
            content: "\n".to_owned(),
        })
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
    let mut runner_ids = Vec::new();
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
                runner_ids = projection
                    .runner_tools
                    .iter()
                    .filter_map(|((current, _), id)| (current == call_id).then_some(*id))
                    .collect::<Vec<_>>();
                projection
                    .runner_tools
                    .retain(|(current, _), _| current != call_id);
            }
            None
        }
        _ => None,
    };
    let operation = match (active_id, runner_ids.is_empty()) {
        (Some(active_id), _) => ThreadOperation::ActiveItemFinalized { active_id, event },
        (None, false) => ThreadOperation::ToolResultFinalized { event, runner_ids },
        (None, true) => ThreadOperation::EventAppended { event },
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
        ToolCallEvent::Function { call_id, .. } => Some(call_id),
    }
}

fn tool_result_call_id(result: &ToolResultEvent) -> Option<&str> {
    match result {
        ToolResultEvent::Custom { call_id, .. } => Some(call_id),
        ToolResultEvent::Function { call_id, .. } => Some(call_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn turn_panic_becomes_failed_terminal_outcome() {
        let outcome = turn_outcome(async {
            panic!("injected provider panic");
            #[allow(unreachable_code)]
            Ok(TurnCompletion::Completed)
        })
        .await;
        assert!(matches!(
            outcome,
            TurnOutcome::Failed { message }
                if message.contains("injected provider panic")
        ));
    }

    #[tokio::test]
    async fn panicking_agent_turn_publishes_failure_and_releases_concurrency_slot() {
        let lifecycle = Arc::new(crate::lifecycle::TurnLifecycle::new());
        let root = atra_protocol::ThreadId(1);
        let thread_id = atra_protocol::ThreadId(2);
        lifecycle.start_agent(thread_id, root).unwrap();
        let (approval_id, approval) = lifecycle.register_approval(thread_id).unwrap();
        for id in 3..10 {
            lifecycle
                .start_agent(atra_protocol::ThreadId(id), root)
                .unwrap();
        }
        assert!(
            lifecycle
                .start_agent(atra_protocol::ThreadId(10), root)
                .is_err()
        );
        let published = Arc::new(tokio::sync::Mutex::new(None));
        let published_outcome = Arc::clone(&published);
        let finish_lifecycle = Arc::clone(&lifecycle);
        complete_registered_turn(
            thread_id,
            async {
                panic!("injected session panic");
                #[allow(unreachable_code)]
                Ok(TurnCompletion::Completed)
            },
            move |outcome| async move {
                *published_outcome.lock().await = Some(outcome);
                Ok(())
            },
            move || async move {
                finish_lifecycle.finish(thread_id);
            },
        )
        .await;

        assert!(matches!(
            published.lock().await.as_ref(),
            Some(TurnOutcome::Failed { message }) if message.contains("injected session panic")
        ));
        assert!(
            lifecycle
                .start_agent(atra_protocol::ThreadId(10), root)
                .is_ok()
        );
        assert!(approval.await.is_err());
        assert!(lifecycle.claim_approval(approval_id).is_err());
    }

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

    #[test]
    fn reasoning_part_boundary_separates_streamed_summaries() {
        let mut projection = TurnProjection::new();
        assert!(projection.reasoning_part_added().is_none());

        let reasoning = projection.id();
        projection.reasoning = Some(reasoning);
        let operation = projection.reasoning_part_added().unwrap();

        assert!(matches!(
            operation,
            ThreadOperation::ActiveTextAppended { id, content }
                if id == reasoning && content == "\n"
        ));
    }
}
