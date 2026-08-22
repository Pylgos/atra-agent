use super::*;
use crate::lifecycle::ActiveTurn;
use futures_util::StreamExt;

struct ExecutionContextGuard {
    contexts: Arc<StdMutex<HashMap<String, ThreadId>>>,
    context: String,
}

impl Drop for ExecutionContextGuard {
    fn drop(&mut self) {
        self.contexts.lock().unwrap().remove(&self.context);
    }
}

fn command_timer_state(timing: &atra_protocol::ProcessTiming) -> CommandTimerState {
    let elapsed_ms = timing.active_elapsed_ms.min(FOREGROUND_TIMEOUT_MS);
    CommandTimerState {
        elapsed_ms,
        remaining_ms: FOREGROUND_TIMEOUT_MS.saturating_sub(elapsed_ms),
        paused: timing.paused,
    }
}

pub(super) enum TurnRequest {
    Send {
        thread_id: ThreadId,
        message: String,
        allow_questions: bool,
    },
    Continue {
        thread_id: ThreadId,
        allow_questions: bool,
    },
    Compact {
        thread_id: ThreadId,
        allow_questions: bool,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TurnCompletion {
    Completed,
    Cancelled,
    Compacted,
}

fn response_event(response: &mut ModelResponse) -> ThreadEventData {
    match response {
        ModelResponse::AssistantMessage { content, phase } => {
            let (projected, todos) = parse_todo_annotation(std::mem::take(content));
            *content = projected.clone();
            ThreadEventData::AssistantMessage(AssistantMessageEvent {
                content: projected,
                phase: *phase,
                todos,
            })
        }
        ModelResponse::WebSearch { item } => {
            ThreadEventData::WebSearch(ItemEvent { item: item.clone() })
        }
        ModelResponse::ToolCall {
            name,
            arguments,
            call_id,
        } => ThreadEventData::ToolCall(ToolCallEvent::Function {
            name: name.clone(),
            arguments: arguments.clone(),
            call_id: call_id.clone(),
        }),
        ModelResponse::CustomToolCall {
            item_id,
            name,
            input,
            call_id,
        } => ThreadEventData::ToolCall(ToolCallEvent::Custom {
            item_id: item_id.clone(),
            name: name.clone(),
            input: input.clone(),
            call_id: call_id.clone(),
        }),
        ModelResponse::Reasoning { summary, opaque } => {
            ThreadEventData::Reasoning(atra_protocol::ReasoningEvent {
                summary: summary.clone(),
                opaque: Some(opaque.clone()),
            })
        }
    }
}

fn tool_result_matches_call(result: &ToolResultEvent, call: &ToolCallEvent) -> bool {
    match (result, call) {
        (
            ToolResultEvent::Custom {
                call_id: result_id, ..
            },
            ToolCallEvent::Custom { call_id, .. },
        ) => result_id == call_id,
        (
            ToolResultEvent::Function {
                call_id: result_id, ..
            },
            ToolCallEvent::Function { call_id, .. },
        ) => result_id == call_id,
        _ => false,
    }
}

impl State {
    pub(super) async fn handle_started_streaming(
        &self,
        request: TurnRequest,
        active: Arc<lifecycle::ActiveTurn>,
        updates: &TurnProjector,
    ) -> Result<TurnCompletion> {
        let thread_id = match &request {
            TurnRequest::Send { thread_id, .. }
            | TurnRequest::Continue { thread_id, .. }
            | TurnRequest::Compact { thread_id, .. } => *thread_id,
        };
        let mut turn = Box::pin(async move {
            match request {
                TurnRequest::Send {
                    thread_id,
                    message,
                    allow_questions,
                } => {
                    self.run_turn(thread_id, message, allow_questions, Some(updates))
                        .await
                }
                TurnRequest::Continue {
                    thread_id,
                    allow_questions,
                } => {
                    self.continue_thread(thread_id, allow_questions, Some(updates))
                        .await
                }
                TurnRequest::Compact {
                    thread_id,
                    allow_questions,
                } => {
                    self.compact_thread(thread_id, allow_questions, Some(updates))
                        .await
                }
            }
        });
        let response = tokio::select! {
            biased;
            () = active.cancelled() => {
                drop(turn);
                return self.complete_turn_cancellation(thread_id, &active).await;
            }
            response = &mut turn => response,
        };
        if active.is_cancelling() {
            return self.complete_turn_cancellation(thread_id, &active).await;
        }
        response
    }
    pub(super) async fn run_turn(
        &self,
        thread_id: ThreadId,
        message: String,
        allow_questions: bool,
        updates: Option<&TurnProjector>,
    ) -> Result<TurnCompletion> {
        self.prepare_thread_for_turn(thread_id, updates).await?;
        let skills = self.sync_skills(thread_id, updates).await?;
        self.sync_runners(thread_id, updates).await?;
        {
            let _mutation = self.lock_mutation().await?;
            self.store
                .name_thread_if_unnamed(thread_id, message.clone())
                .await
                .context("failed to name thread")?;
            let metadata = self.store.thread(thread_id).await?;
            self.views
                .update_thread_metadata(thread_id, metadata)
                .await?;
        }
        self.sync_workspace_instructions(thread_id, updates).await?;
        let (message, invocations) = skills::resolve_invocations(&message, &skills.skills);
        let mut events = invocations
            .into_iter()
            .map(|invocation| {
                ThreadEventData::SkillInvocation(atra_protocol::SkillInvocationEvent {
                    path: format!("$ATRA_SKILLS/{}/SKILL.md", invocation.name),
                    name: invocation.name,
                    instructions: invocation.instructions,
                })
            })
            .collect::<Vec<_>>();
        events.push(ThreadEventData::UserMessage(MessageEvent {
            content: message,
        }));
        {
            let _mutation = self.lock_mutation().await?;
            let saved = self
                .store
                .append_all(thread_id, events.clone())
                .await
                .context("failed to save user request")?;
            if let Some(updates) = updates {
                for (sequence, data) in saved.into_iter().zip(events) {
                    send_thread_event(updates, ThreadEvent { sequence, data }).await?;
                }
            }
        }
        self.continue_turn(thread_id, allow_questions, updates)
            .await
    }

    pub(super) async fn continue_thread(
        &self,
        thread_id: ThreadId,
        allow_questions: bool,
        updates: Option<&TurnProjector>,
    ) -> Result<TurnCompletion> {
        self.prepare_thread_for_turn(thread_id, updates).await?;
        self.sync_skills(thread_id, updates).await?;
        self.sync_runners(thread_id, updates).await?;
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load thread history")?;
        let resumable = events.iter().rev().find(|event| {
            matches!(
                event.data,
                ThreadEventData::UserMessage(_)
                    | ThreadEventData::AssistantMessage(_)
                    | ThreadEventData::ToolCall(_)
                    | ThreadEventData::ToolResult(_)
                    | ThreadEventData::Compaction(_)
            )
        });
        match resumable.map(|event| &event.data) {
            Some(
                ThreadEventData::UserMessage(_)
                | ThreadEventData::ToolResult(_)
                | ThreadEventData::Compaction(_),
            ) => {}
            Some(ThreadEventData::AssistantMessage(message))
                if message.phase == AssistantMessagePhase::Commentary => {}
            Some(ThreadEventData::AssistantMessage(_)) => bail!("thread turn is already complete"),
            Some(ThreadEventData::ToolCall(_)) => unreachable!(),
            None => bail!("thread has no resumable history"),
            _ => unreachable!(),
        }
        self.sync_workspace_instructions(thread_id, updates).await?;
        self.continue_turn(thread_id, allow_questions, updates)
            .await
    }

    pub(super) async fn prepare_thread_for_turn(
        &self,
        thread_id: ThreadId,
        updates: Option<&TurnProjector>,
    ) -> Result<()> {
        let events = self
            .store
            .active_tool_events(thread_id)
            .await
            .context("failed to load active tool history")?;
        let mut pending = Vec::new();
        for event in &events {
            match &event.data {
                ThreadEventData::ToolCall(call) => pending.push(call.clone()),
                ThreadEventData::ToolResult(result) => {
                    if let Some(index) = pending
                        .iter()
                        .position(|call| tool_result_matches_call(result, call))
                    {
                        pending.remove(index);
                    }
                }
                _ => {}
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        for call in pending {
            let (name, call_id, custom) = match &call {
                ToolCallEvent::Custom { name, call_id, .. } => {
                    (name.as_str(), call_id.as_str(), true)
                }
                ToolCallEvent::Function { name, call_id, .. } => {
                    (name.as_str(), call_id.as_str(), false)
                }
            };
            self.save_tool_result(
                thread_id,
                name,
                call_id,
                ToolOutcome::text("tool execution was interrupted before completion".to_owned()),
                custom,
                updates,
            )
            .await
            .context("failed to save interrupted tool result")?;
        }
        Ok(())
    }

    async fn complete_turn_cancellation(
        &self,
        thread_id: ThreadId,
        active: &ActiveTurn,
    ) -> Result<TurnCompletion> {
        let publish = self.views.start_cancellation(thread_id).await;
        let stop = active.request_cancellation().await;
        self.turns.clear_interactions(thread_id);
        let cleanup = self.prepare_thread_for_turn(thread_id, None).await;
        let result = match (publish, stop, cleanup) {
            (Ok(()), Ok(()), Ok(())) => Ok(()),
            (Err(publish), Ok(()), Ok(())) => Err(publish),
            (Ok(()), Err(stop), Ok(())) => Err(stop),
            (Ok(()), Ok(()), Err(cleanup)) => Err(cleanup),
            (publish, stop, cleanup) => {
                let mut failures = Vec::new();
                if let Err(error) = publish {
                    failures.push(format!("failed to publish cancellation: {error:#}"));
                }
                if let Err(error) = stop {
                    failures.push(format!("failed to stop active process: {error:#}"));
                }
                if let Err(error) = cleanup {
                    failures.push(format!("failed to clean up cancelled turn: {error:#}"));
                }
                Err(anyhow!(failures.join("; ")))
            }
        };
        result?;
        Ok(TurnCompletion::Cancelled)
    }

    pub(super) async fn compact_thread(
        &self,
        thread_id: ThreadId,
        allow_questions: bool,
        updates: Option<&TurnProjector>,
    ) -> Result<TurnCompletion> {
        self.prepare_thread_for_turn(thread_id, updates).await?;
        self.sync_skills(thread_id, updates).await?;
        self.sync_runners(thread_id, updates).await?;
        self.sync_workspace_instructions(thread_id, updates).await?;
        let prompt_cache_key = format!(
            "{:x}",
            Sha256::digest(format!("{}-{thread_id}", self.prompt_cache_namespace))
        );
        let model_tools = model_tools(allow_questions);
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load model history")?;
        let (provider_id, model, reasoning_effort) = self
            .store
            .thread_model(thread_id)
            .await
            .context("failed to load thread model")?;
        let provider = self.provider(&provider_id)?;
        let selected_model = provider
            .models()
            .await?
            .into_iter()
            .find(|candidate| candidate.id == model);
        let context_window = selected_model
            .as_ref()
            .and_then(|model| model.context_window);
        let model_request = model::ModelRequest {
            model: &model,
            reasoning_effort: &reasoning_effort,
            instructions: model::BASE_INSTRUCTIONS,
            tools: &model_tools,
            events: &events,
            prompt_cache_key: &prompt_cache_key,
        };
        if !self
            .compact_history(
                thread_id,
                provider.as_ref(),
                &prompt_cache_key,
                &model_request,
                context_window,
                updates,
            )
            .await?
        {
            bail!("model returned an empty compaction");
        }
        Ok(TurnCompletion::Compacted)
    }

    async fn compact_history(
        &self,
        thread_id: ThreadId,
        provider: &model::Provider,
        session_id: &str,
        model_request: &model::ModelRequest<'_>,
        context_window: Option<i64>,
        updates: Option<&TurnProjector>,
    ) -> Result<bool> {
        self.append_event(
            thread_id,
            ThreadEventData::ModelRequest(ModelRequestEvent {
                kind: ModelRequestKind::Compaction,
                context_window,
            }),
            updates,
        )
        .await
        .context("failed to save compaction request")?;
        let checkpoint_id = {
            let _mutation = self.lock_mutation().await?;
            let checkpoint_id = self
                .store
                .create_checkpoint(thread_id, checkpoint_time_ms(), "compaction".to_owned())
                .await
                .context("failed to checkpoint history before compaction")?;
            if let Some(updates) = updates {
                let checkpoint = self.store.checkpoint(checkpoint_id).await?;
                updates.checkpoint_added(checkpoint).await?;
            }
            checkpoint_id
        };
        let replacement =
            if let Some(state) = provider.server_compact(session_id, model_request).await? {
                atra_protocol::CompactionReplacement::Opaque { state }
            } else {
                let Some(content) = generic_compaction(provider, session_id, model_request).await?
                else {
                    return Ok(false);
                };
                atra_protocol::CompactionReplacement::Summary { content }
            };
        let workspace_event = match workspace_instructions(model_request.events) {
            WorkspaceInstructions::Untracked => None,
            WorkspaceInstructions::Present(content) => Some(InstructionEvent::Initial(content)),
            WorkspaceInstructions::Removed => Some(InstructionEvent::Removal),
        };
        {
            let _mutation = self.lock_mutation().await?;
            self.store
                .replace_with_compaction(
                    thread_id,
                    CompactionEvent {
                        replacement,
                        checkpoint_id,
                    },
                    workspace_event,
                    skill_event(model_request.events),
                    runner_event(model_request.events),
                )
                .await
                .context("failed to replace history after compaction")?;
            if let Some(updates) = updates {
                let events = protocol_events(self.store.events(thread_id).await?);
                updates.history_replaced(events).await?;
            }
        }
        Ok(true)
    }

    pub(super) async fn continue_turn(
        &self,
        thread_id: ThreadId,
        allow_questions: bool,
        updates: Option<&TurnProjector>,
    ) -> Result<TurnCompletion> {
        let prompt_cache_key = format!(
            "{:x}",
            Sha256::digest(format!("{}-{thread_id}", self.prompt_cache_namespace))
        );
        let (provider_id, model, reasoning_effort) = self
            .store
            .thread_model(thread_id)
            .await
            .context("failed to load thread model")?;
        let provider = self.provider(&provider_id)?;
        let model_tools = model_tools(allow_questions);
        loop {
            self.sync_runners(thread_id, updates).await?;
            let mut events = self
                .store
                .events(thread_id)
                .await
                .context("failed to load model history")?;
            let selected_model = provider
                .models()
                .await?
                .into_iter()
                .find(|candidate| candidate.id == model);
            let context_window = selected_model
                .as_ref()
                .and_then(|model| model.context_window);
            let auto_compact_token_limit =
                selected_model.and_then(|model| model.auto_compact_token_limit);
            let active_history_start = events
                .iter()
                .rposition(|event| matches!(event.data, ThreadEventData::Compaction(_)))
                .map_or(0, |index| index + 1);
            let active_tokens = events[active_history_start..]
                .iter()
                .rev()
                .find_map(|event| match &event.data {
                    ThreadEventData::TokenUsage(event) => Some(&event.usage),
                    _ => None,
                })
                .and_then(|usage| usage["total_tokens"].as_i64());
            if active_tokens
                .zip(auto_compact_token_limit)
                .is_some_and(|(tokens, limit)| tokens >= limit)
            {
                let model_request = model::ModelRequest {
                    model: &model,
                    reasoning_effort: &reasoning_effort,
                    instructions: model::BASE_INSTRUCTIONS,
                    tools: &model_tools,
                    events: &events,
                    prompt_cache_key: &prompt_cache_key,
                };
                if self
                    .compact_history(
                        thread_id,
                        provider.as_ref(),
                        &prompt_cache_key,
                        &model_request,
                        context_window,
                        updates,
                    )
                    .await?
                {
                    events = self
                        .store
                        .events(thread_id)
                        .await
                        .context("failed to reload compacted model history")?;
                }
            }
            let model_request = model::ModelRequest {
                model: &model,
                reasoning_effort: &reasoning_effort,
                instructions: model::BASE_INSTRUCTIONS,
                tools: &model_tools,
                events: &events,
                prompt_cache_key: &prompt_cache_key,
            };
            let request_sequence = self
                .append_event(
                    thread_id,
                    ThreadEventData::ModelRequest(ModelRequestEvent {
                        kind: ModelRequestKind::Response,
                        context_window,
                    }),
                    updates,
                )
                .await
                .context("failed to save model request")?;
            let mut responses = VecDeque::new();
            let mut stream = provider.stream(&prompt_cache_key, &model_request).await?;
            let mut completed = false;
            let mut stream_error = None;
            let mut partial_text = String::new();
            while let Some(event) = stream.next().await {
                let event = match event {
                    Ok(event) => event,
                    Err(error) => {
                        stream_error = Some(error);
                        break;
                    }
                };
                match event {
                    model::ModelEvent::Update(event) => {
                        if let model::ModelStreamEvent::AssistantDelta { content, .. } = &event {
                            partial_text.push_str(content);
                        }
                        if let Some(updates) = updates {
                            updates.apply_update(event).await?;
                        }
                    }
                    model::ModelEvent::OutputItemDone { mut response } => {
                        let mut events = Vec::new();
                        if let Some(response) = &mut response {
                            if matches!(response, ModelResponse::AssistantMessage { .. }) {
                                partial_text.clear();
                            }
                            events.push(response_event(response));
                        }
                        if events.is_empty() {
                            continue;
                        }
                        {
                            let _mutation = self.lock_mutation().await?;
                            let saved = self
                                .store
                                .append_all(thread_id, events.clone())
                                .await
                                .context("failed to save completed model output item")?;
                            if let Some(updates) = updates {
                                for (sequence, data) in saved.into_iter().zip(events) {
                                    send_thread_event(updates, ThreadEvent { sequence, data })
                                        .await?;
                                }
                            }
                        }
                        if let Some(
                            response @ (ModelResponse::AssistantMessage { .. }
                            | ModelResponse::ToolCall { .. }
                            | ModelResponse::CustomToolCall { .. }),
                        ) = response
                        {
                            responses.push_back(response);
                        }
                    }
                    model::ModelEvent::Completed {
                        token_usage,
                        rate_limits,
                    } => {
                        if let Some(usage) = token_usage {
                            self.append_event(
                                thread_id,
                                ThreadEventData::TokenUsage(TokenUsageEvent {
                                    request_sequence,
                                    usage,
                                }),
                                updates,
                            )
                            .await
                            .context("failed to save token usage")?;
                        }
                        if !rate_limits.is_empty() {
                            self.append_event(
                                thread_id,
                                ThreadEventData::RateLimits(RateLimitsEvent {
                                    request_sequence,
                                    snapshots: serde_json::to_value(rate_limits)
                                        .map_err(|error| anyhow!(error))?,
                                }),
                                updates,
                            )
                            .await
                            .context("failed to save rate limits")?;
                        }
                        completed = true;
                    }
                }
            }
            if !completed && stream_error.is_none() {
                stream_error = Some(anyhow!("model stream ended before completion"));
            }
            if stream_error.is_some() && !partial_text.is_empty() {
                self.append_event(
                    thread_id,
                    ThreadEventData::AssistantMessage(AssistantMessageEvent {
                        content: std::mem::take(&mut partial_text),
                        phase: AssistantMessagePhase::Commentary,
                        todos: Vec::new(),
                    }),
                    updates,
                )
                .await
                .context("failed to save partial assistant output")?;
            }
            let completed_response = self
                .execute_model_responses(
                    provider.as_ref(),
                    &model,
                    thread_id,
                    responses,
                    allow_questions,
                    updates,
                )
                .await?;
            if let Some(error) = stream_error {
                return Err(error);
            }
            if completed_response {
                return Ok(TurnCompletion::Completed);
            }
        }
    }

    pub(super) async fn sync_workspace_instructions(
        &self,
        thread_id: ThreadId,
        updates: Option<&TurnProjector>,
    ) -> Result<()> {
        let content = self.read_workspace_instructions().await?;
        let events = self
            .store
            .latest_event(thread_id, "workspace_instructions")
            .await
            .context("failed to load workspace instruction state")?;
        let previous = events
            .as_ref()
            .map_or(WorkspaceInstructions::Untracked, |event| {
                workspace_instructions(std::slice::from_ref(event))
            });
        if matches!(
            (&previous, &content),
            (WorkspaceInstructions::Present(previous), Some(content)) if previous == content
        ) || matches!(
            (&previous, &content),
            (
                WorkspaceInstructions::Removed | WorkspaceInstructions::Untracked,
                None
            )
        ) {
            return Ok(());
        }

        let event = match (previous, content) {
            (_, None) => InstructionEvent::Removal,
            (WorkspaceInstructions::Present(_), Some(content)) => {
                InstructionEvent::Replacement(content)
            }
            (WorkspaceInstructions::Untracked | WorkspaceInstructions::Removed, Some(content)) => {
                InstructionEvent::Initial(content)
            }
        };
        self.append_event(
            thread_id,
            ThreadEventData::WorkspaceInstructions(event),
            updates,
        )
        .await
        .context("failed to save workspace instructions")?;
        Ok(())
    }

    pub(super) async fn sync_skills(
        &self,
        thread_id: ThreadId,
        updates: Option<&TurnProjector>,
    ) -> Result<Arc<skills::SkillGeneration>> {
        let generation = self.collect_skill_generation().await?;

        self.runners
            .sync_skills(&self.skill_store, &generation)
            .await?;
        *self.skill_generation.lock().await = Some(Arc::clone(&generation));

        let events = self
            .store
            .latest_event(thread_id, "skills")
            .await
            .context("failed to load skill state")?;
        let previous = events
            .as_ref()
            .map_or(WorkspaceInstructions::Untracked, |event| {
                current_skills(std::slice::from_ref(event))
            });
        if matches!(
            (&previous, &generation.prompt),
            (WorkspaceInstructions::Present(previous), Some(content)) if previous == content
        ) || matches!(
            (&previous, &generation.prompt),
            (
                WorkspaceInstructions::Removed | WorkspaceInstructions::Untracked,
                None
            )
        ) {
            return Ok(generation);
        }
        let event = match (&previous, &generation.prompt) {
            (_, None) => InstructionEvent::Removal,
            (WorkspaceInstructions::Present(_), Some(content)) => {
                InstructionEvent::Replacement(content.clone())
            }
            (WorkspaceInstructions::Untracked | WorkspaceInstructions::Removed, Some(content)) => {
                InstructionEvent::Initial(content.clone())
            }
        };
        self.append_event(thread_id, ThreadEventData::Skills(event), updates)
            .await
            .context("failed to save skills")?;
        Ok(generation)
    }

    pub(super) async fn collect_skill_generation(&self) -> Result<Arc<skills::SkillGeneration>> {
        let workspace = self.workspace.clone();
        let data_home = self.data_home.clone();
        let store = self.skill_store.clone();
        Ok(Arc::new(
            tokio::task::spawn_blocking(move || skills::collect(&workspace, &data_home, &store))
                .await
                .context("skill collection task failed")??,
        ))
    }

    pub(super) async fn sync_runners(
        &self,
        thread_id: ThreadId,
        updates: Option<&TurnProjector>,
    ) -> Result<()> {
        let runners = self.runners.list().await?;
        let events = self
            .store
            .latest_event(thread_id, "runners")
            .await
            .context("failed to load runner state")?;
        let previous = events
            .as_ref()
            .and_then(|event| current_runners(std::slice::from_ref(event)));
        if previous.as_ref() == Some(&runners) {
            return Ok(());
        }
        self.append_event(
            thread_id,
            ThreadEventData::Runners(if previous.is_some() {
                RunnersEvent::Replacement(runners.clone())
            } else {
                RunnersEvent::Initial(runners.clone())
            }),
            updates,
        )
        .await
        .context("failed to save runners")?;
        Ok(())
    }

    pub(super) async fn read_workspace_instructions(&self) -> Result<Option<String>> {
        for filename in ["AGENTS.override.md", "AGENTS.md"] {
            let path = self.workspace.join(filename);
            match tokio::fs::metadata(&path).await {
                Ok(metadata) if !metadata.is_file() => continue,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to inspect workspace instructions {}",
                            path.display()
                        )
                    });
                }
            }
            let mut data = match tokio::fs::read(&path).await {
                Ok(data) => data,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to read workspace instructions {}", path.display())
                    });
                }
            };
            data.truncate(WORKSPACE_INSTRUCTIONS_MAX_BYTES);
            let content = String::from_utf8_lossy(&data).trim().to_owned();
            return Ok((!content.is_empty()).then_some(content));
        }
        Ok(None)
    }

    pub(super) async fn execute_model_responses(
        &self,
        provider: &model::Provider,
        model_id: &str,
        thread_id: ThreadId,
        mut responses: VecDeque<ModelResponse>,
        allow_questions: bool,
        updates: Option<&TurnProjector>,
    ) -> Result<bool> {
        let mut final_answer = None;
        let mut needs_follow_up = false;
        while let Some(response) = responses.pop_front() {
            match response {
                ModelResponse::AssistantMessage { content, phase } => {
                    if phase == AssistantMessagePhase::FinalAnswer {
                        final_answer = Some(content);
                    }
                }
                ModelResponse::WebSearch { .. } => {}
                ModelResponse::ToolCall {
                    name,
                    arguments,
                    call_id,
                } => {
                    let validated = match crate::tools::validate_function_tool(
                        &name,
                        arguments.clone(),
                        allow_questions,
                    ) {
                        Ok(validated) => validated,
                        Err(error) => {
                            needs_follow_up = true;
                            self.save_tool_error(
                                thread_id,
                                &name,
                                &call_id,
                                error.tool_result(&name),
                                false,
                                updates,
                            )
                            .await?;
                            continue;
                        }
                    };
                    match validated {
                        crate::tools::ValidatedFunctionTool::Questions(questions) => {
                            let (request_id, answer) = self.turns.register_question(thread_id)?;
                            updates
                                .context("question requires a streaming turn")?
                                .interaction_requested(
                                    atra_protocol::PendingInteraction::Questions(
                                        atra_protocol::PendingQuestionRequest {
                                            id: request_id,
                                            questions,
                                        },
                                    ),
                                )
                                .await?;
                            let answers = answer
                                .await
                                .context("question was removed before it was answered")?;
                            needs_follow_up = true;
                            self.save_tool_result(
                                thread_id,
                                &name,
                                &call_id,
                                ToolOutcome {
                                    result: serde_json::to_value(&answers)
                                        .context("failed to encode question answers")?,
                                    artifacts: Vec::new(),
                                },
                                false,
                                updates,
                            )
                            .await?;
                            continue;
                        }
                        crate::tools::ValidatedFunctionTool::Command(command) => {
                            let operation = OperationContext {
                                call_id: call_id.clone(),
                                index: 1,
                                label: "Command".to_owned(),
                            };
                            let outcome = self
                                .approve_and_execute(thread_id, &name, command, &operation, updates)
                                .await?;
                            needs_follow_up = true;
                            self.save_tool_result(
                                thread_id, &name, &call_id, outcome, false, updates,
                            )
                            .await?;
                            continue;
                        }
                        crate::tools::ValidatedFunctionTool::Provider => {}
                    }
                    let result = provider
                        .execute_tool(model_id, &name, &arguments)
                        .await?
                        .with_context(|| format!("model requested unsupported tool {name}"))?;
                    needs_follow_up = true;
                    if matches!(name.as_str(), "web_search" | "web_fetch") {
                        self.append_event(
                            thread_id,
                            ThreadEventData::WebSearch(ItemEvent {
                                item: serde_json::json!({
                                    "action": {
                                        "type": &name,
                                        "arguments": &arguments,
                                    },
                                    "result": &result,
                                }),
                            }),
                            updates,
                        )
                        .await?;
                    }
                    self.save_tool_result(
                        thread_id,
                        &name,
                        &call_id,
                        ToolOutcome {
                            result,
                            artifacts: Vec::new(),
                        },
                        false,
                        updates,
                    )
                    .await?;
                }
                ModelResponse::CustomToolCall {
                    name,
                    input,
                    call_id,
                    ..
                } => {
                    needs_follow_up = true;
                    let operations = match crate::tools::validate_custom_tool(&name, &input) {
                        Ok(operations) => operations,
                        Err(error) => {
                            self.save_tool_error(
                                thread_id,
                                &name,
                                &call_id,
                                error.tool_result(&name),
                                true,
                                updates,
                            )
                            .await?;
                            continue;
                        }
                    };
                    let mut results = Vec::new();
                    let mut artifacts = Vec::new();
                    for (index, operation) in operations.into_iter().enumerate() {
                        let operation_index = index + 1;
                        let runner = operation.runner().to_owned();
                        let operation_name = "command";
                        let result_label = "Command".to_owned();
                        let operation_context = OperationContext {
                            call_id: call_id.clone(),
                            index: operation_index,
                            label: result_label.clone(),
                        };
                        let outcome = self
                            .approve_and_execute(
                                thread_id,
                                operation_name,
                                operation,
                                &operation_context,
                                updates,
                            )
                            .await?;
                        let result = outcome
                            .result
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| outcome.result.to_string());
                        results.push(format!(
                            "Operation {} [{runner}] {result_label}:\n{}",
                            operation_index, result
                        ));
                        let artifact = ToolArtifact::RunnerOperation(RunnerOperationArtifact {
                            operation: operation_index,
                            runner: runner.clone(),
                            label: result_label,
                            result: serde_json::Value::String(result),
                            artifacts: outcome.artifacts,
                        });
                        send_operation_update(
                            &operation_context,
                            updates,
                            &runner,
                            RunnerOperationUpdate::Completed {
                                artifact: artifact.clone(),
                            },
                        )
                        .await?;
                        artifacts.push(artifact);
                    }
                    self.save_tool_result(
                        thread_id,
                        &name,
                        &call_id,
                        ToolOutcome {
                            result: serde_json::Value::String(results.join("\n\n")),
                            artifacts,
                        },
                        true,
                        updates,
                    )
                    .await?;
                }
                ModelResponse::Reasoning { .. } => {}
            }
        }
        Ok(!needs_follow_up && final_answer.is_some())
    }

    async fn save_tool_error(
        &self,
        thread_id: ThreadId,
        name: &str,
        call_id: &str,
        error: String,
        custom: bool,
        updates: Option<&TurnProjector>,
    ) -> Result<()> {
        self.save_tool_result(
            thread_id,
            name,
            call_id,
            ToolOutcome::text(error),
            custom,
            updates,
        )
        .await?;
        Ok(())
    }

    pub(super) async fn approve_and_execute(
        &self,
        thread_id: ThreadId,
        name: &str,
        arguments: atra_protocol::RunnerCommand,
        operation: &OperationContext,
        updates: Option<&TurnProjector>,
    ) -> Result<ToolOutcome> {
        let runner = self.runners.get(arguments.runner()).await?;
        let decision = if runner.approval().await == ApprovalPolicy::Ask {
            let arguments_json =
                serde_json::to_value(&arguments).context("failed to encode approval arguments")?;
            let (approval_id, approval) = self.turns.register_approval(thread_id)?;
            updates
                .context("approval requires a streaming turn")?
                .interaction_requested(atra_protocol::PendingInteraction::Approval(
                    atra_protocol::PendingApproval::new(
                        approval_id,
                        name.to_owned(),
                        arguments_json,
                        Some(operation.index),
                        Some(operation.label.clone()),
                    ),
                ))
                .await?;
            Some(
                approval
                    .await
                    .context("approval was removed before it was resolved")?,
            )
        } else {
            None
        };
        let allowed = decision.as_ref().is_none_or(|decision| decision.allowed);
        let result = if allowed {
            self.execute(thread_id, arguments, operation, updates)
                .await?
        } else {
            let reason = decision.and_then(|decision| decision.reason);
            let output = match reason {
                Some(reason) => format!("user denied the tool call: {reason}"),
                None => "user denied the tool call".to_owned(),
            };
            ToolOutcome::text(output)
        };
        Ok(result)
    }

    pub(super) async fn claim_approval(
        &self,
        approval_id: InteractionId,
    ) -> Result<(ThreadId, lifecycle::InteractionWaiter)> {
        self.turns.claim_approval(approval_id)
    }

    pub(super) async fn claim_questions(
        &self,
        request_id: InteractionId,
    ) -> Result<(ThreadId, lifecycle::InteractionWaiter)> {
        self.turns.claim_questions(request_id)
    }

    pub(super) async fn execute(
        &self,
        thread_id: ThreadId,
        arguments: atra_protocol::RunnerCommand,
        operation: &OperationContext,
        updates: Option<&TurnProjector>,
    ) -> Result<ToolOutcome> {
        let runner_name = arguments.runner().to_owned();
        let command = arguments.command().to_owned();
        let started_at_ms = checkpoint_time_ms();
        let runner = self.runners.get(arguments.runner()).await?;
        let active = self
            .turns
            .get(thread_id)
            .context("thread has no active turn")?;
        let process_id = self
            .runners
            .generate_process_id(thread_id, &runner_name)
            .await;
        let execution_context = format!("{}-{}", thread_id, atra_id::generate().replace(' ', "-"));
        self.execution_contexts
            .lock()
            .unwrap()
            .insert(execution_context.clone(), thread_id);
        let _execution_context = ExecutionContextGuard {
            contexts: Arc::clone(&self.execution_contexts),
            context: execution_context.clone(),
        };
        let started = runner
            .start_command(
                arguments.command().to_owned(),
                thread_id,
                &process_id,
                Some(execution_context),
            )
            .await?;
        let process_handle = started.handle;
        active.set_process(Arc::clone(&runner), process_handle.clone());
        send_operation_update(
            operation,
            updates,
            &runner_name,
            RunnerOperationUpdate::CommandStarted {
                timer: command_timer_state(&started.timing),
            },
        )
        .await?;
        let mut collected = None;
        let mut patch_results = Vec::new();
        let response = loop {
            match runner
                .wait(process_handle.clone(), FOREGROUND_TIMEOUT_MS)
                .await?
            {
                WaitOutcome::Running {
                    output,
                    patch_results: result_patch_results,
                    spawned_processes,
                    timing,
                    ..
                } => {
                    self.register_spawned_processes(thread_id, &runner_name, spawned_processes)
                        .await?;
                    let delta_content = output.content.clone();
                    let delta_omitted_bytes = output.omitted_bytes;
                    append_command_output(&mut collected, output);
                    let timer = command_timer_state(&timing);
                    send_operation_output(
                        operation,
                        updates,
                        delta_content,
                        delta_omitted_bytes,
                        timer.clone(),
                    )
                    .await?;
                    patch_results.extend(result_patch_results);
                    if timer.remaining_ms == 0 {
                        break WaitOutcome::Running {
                            process_handle: process_handle.clone(),
                            output: collected.take().unwrap(),
                            patch_results,
                            spawned_processes: Vec::new(),
                            timing,
                        };
                    }
                }
                WaitOutcome::Finished {
                    output,
                    exit_code,
                    patch_results: result_patch_results,
                    spawned_processes,
                } => {
                    self.register_spawned_processes(thread_id, &runner_name, spawned_processes)
                        .await?;
                    append_command_output(&mut collected, output);
                    patch_results.extend(result_patch_results);
                    break WaitOutcome::Finished {
                        output: collected.take().unwrap(),
                        exit_code,
                        patch_results,
                        spawned_processes: Vec::new(),
                    };
                }
            }
        };
        let response = match response {
            WaitOutcome::Running {
                process_handle,
                output,
                patch_results,
                ..
            } => {
                let subscription = runner.subscribe(process_handle.clone()).await?;
                self.register_managed_process(
                    ProcessKey {
                        thread_id,
                        runner: runner_name.clone(),
                        process_id: process_id.clone(),
                    },
                    ProcessRecord {
                        handle: process_handle,
                        command: command.clone(),
                        started_at_ms,
                    },
                    subscription,
                )
                .await?;
                active.clear_process();
                CommandOutcome::Running {
                    process_id,
                    output,
                    patch_results,
                }
            }
            WaitOutcome::Finished {
                output,
                exit_code,
                patch_results,
                ..
            } => {
                active.clear_process();
                CommandOutcome::Finished {
                    output,
                    exit_code,
                    patch_results,
                }
            }
        };
        let artifact = command_artifact(&response, &runner_name);
        let mut artifacts = vec![ToolArtifact::CommandExecution(artifact)];
        match &response {
            CommandOutcome::Running { patch_results, .. }
            | CommandOutcome::Finished { patch_results, .. } => artifacts.extend(
                patch_results
                    .iter()
                    .cloned()
                    .map(ToolArtifact::PatchOperations),
            ),
        }
        Ok(ToolOutcome {
            result: serde_json::Value::String(format_command_response(&runner_name, response)),
            artifacts,
        })
    }

    pub(super) async fn save_tool_result(
        &self,
        thread_id: ThreadId,
        name: &str,
        call_id: &str,
        outcome: ToolOutcome,
        custom: bool,
        updates: Option<&TurnProjector>,
    ) -> Result<()> {
        let data = if custom {
            ThreadEventData::ToolResult(ToolResultEvent::Custom {
                name: name.to_owned(),
                call_id: call_id.to_owned(),
                result: outcome.result,
                artifacts: outcome.artifacts,
            })
        } else {
            ThreadEventData::ToolResult(ToolResultEvent::Function {
                name: name.to_owned(),
                call_id: call_id.to_owned(),
                result: outcome.result,
                artifacts: outcome.artifacts,
            })
        };
        self.append_event(thread_id, data, updates)
            .await
            .context("failed to save tool result")?;
        Ok(())
    }

    pub(super) async fn append_event(
        &self,
        thread_id: ThreadId,
        data: ThreadEventData,
        updates: Option<&TurnProjector>,
    ) -> Result<EventSequence> {
        let _mutation = self.lock_mutation().await?;
        let sequence = self.store.append(thread_id, data.clone()).await?;
        if let Some(updates) = updates {
            send_thread_event(updates, ThreadEvent { sequence, data }).await?;
        }
        Ok(sequence)
    }
}

async fn generic_compaction(
    provider: &model::Provider,
    session_id: &str,
    request: &model::ModelRequest<'_>,
) -> Result<Option<String>> {
    let events = compaction_events(request.events);
    let request = compaction_request(request, &events);
    let mut stream = provider.stream(session_id, &request).await?;
    let mut summary = String::new();
    let mut completed = false;
    while let Some(event) = stream.next().await {
        match event? {
            model::ModelEvent::OutputItemDone {
                response: Some(model::ModelResponse::AssistantMessage { content, .. }),
            } => summary.push_str(&content),
            model::ModelEvent::OutputItemDone {
                response:
                    Some(
                        model::ModelResponse::ToolCall { .. }
                        | model::ModelResponse::CustomToolCall { .. }
                        | model::ModelResponse::WebSearch { .. },
                    ),
            } => bail!("model attempted to call a tool during compaction"),
            model::ModelEvent::Completed { .. } => completed = true,
            model::ModelEvent::Update(_)
            | model::ModelEvent::OutputItemDone { response: None }
            | model::ModelEvent::OutputItemDone {
                response: Some(model::ModelResponse::Reasoning { .. }),
            } => {}
        }
    }
    ensure!(completed, "model compaction stream ended before completion");
    Ok((!summary.trim().is_empty()).then_some(summary))
}

fn compaction_request<'a>(
    request: &model::ModelRequest<'a>,
    events: &'a [crate::storage::Event],
) -> model::ModelRequest<'a> {
    model::ModelRequest { events, ..*request }
}

fn compaction_events(events: &[crate::storage::Event]) -> Vec<crate::storage::Event> {
    let mut compact = events.to_vec();
    let sequence = compact.last().map_or(EventSequence(0), |event| {
        EventSequence(event.sequence.0.saturating_add(1))
    });
    compact.push(crate::storage::Event {
        sequence,
        data: ThreadEventData::UserMessage(MessageEvent {
            content: "Create a faithful handoff summary of every message before this one for another coding assistant. Preserve all decisions, constraints, code changes, file paths, relevant tool results, unfinished work, and exact literals such as identifiers, names, numbers, and markers. Do not claim that prior context is absent. Do not call tools. Return only the summary.".to_owned(),
        }),
    });
    compact
}

#[cfg(test)]
mod compaction_tests {
    use super::*;

    #[test]
    fn generic_compaction_preserves_the_request_and_appends_only_the_trigger() {
        let events = vec![crate::storage::Event {
            sequence: EventSequence(7),
            data: ThreadEventData::UserMessage(MessageEvent {
                content: "original".to_owned(),
            }),
        }];
        let tools = crate::tools::model_tools(true);
        let request = model::ModelRequest {
            model: "model",
            reasoning_effort: "exact",
            instructions: "instructions",
            tools: &tools,
            events: &events,
            prompt_cache_key: "cache",
        };
        let compact_events = compaction_events(request.events);
        let compact = compaction_request(&request, &compact_events);

        assert_eq!(compact.events.len(), events.len() + 1);
        assert_eq!(&compact.events[..events.len()], events);
        assert!(matches!(
            compact.events.last().unwrap().data,
            ThreadEventData::UserMessage(_)
        ));
        assert_eq!(compact.model, request.model);
        assert_eq!(compact.reasoning_effort, request.reasoning_effort);
        assert_eq!(compact.instructions, request.instructions);
        assert!(std::ptr::eq(compact.tools, request.tools));
        assert_eq!(compact.prompt_cache_key, request.prompt_cache_key);
    }
}

async fn send_thread_event(updates: &TurnProjector, event: ThreadEvent) -> Result<()> {
    updates.event_finalized(event).await
}
