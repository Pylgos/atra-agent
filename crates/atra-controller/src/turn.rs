use super::*;

impl State {
    pub(super) async fn handle_streaming(
        &self,
        request: TurnRequest,
        updates: &mpsc::UnboundedSender<ModelStreamEvent>,
    ) -> Result<ControllerResponse> {
        let thread_id = match &request {
            TurnRequest::ThreadSend { thread_id, .. }
            | TurnRequest::ThreadContinue { thread_id } => *thread_id,
        };
        let active = self.turns.start(thread_id).await?;
        updates
            .send(ModelStreamEvent::TurnStarted { thread_id })
            .context("turn stream closed before turn started")?;
        let mut cancel_requested = active.cancel_requested();
        let mut cancellation = active.cancellation();
        let mut turn = Box::pin(async {
            match request {
                TurnRequest::ThreadSend { thread_id, message } => {
                    self.run_turn(thread_id, message, Some(updates)).await
                }
                TurnRequest::ThreadContinue { thread_id } => {
                    self.continue_thread(thread_id, Some(updates)).await
                }
            }
        });
        let completed = tokio::select! {
            biased;
            changed = cancel_requested.changed() => {
                changed.context("turn cancellation channel closed")?;
                None
            }
            response = &mut turn => Some(response),
        };
        let mut response = if let Some(response) = completed {
            response
        } else {
            drop(turn);
            cancellation
                .changed()
                .await
                .context("turn cancellation channel closed")?;
            match cancellation
                .borrow()
                .clone()
                .expect("cancellation completed")
            {
                Ok(()) => Ok(ControllerResponse::ThreadCancelled),
                Err(message) => Err(anyhow!(message)),
            }
        };
        if active.is_cancelling() && !matches!(response, Ok(ControllerResponse::ThreadCancelled)) {
            if !*cancel_requested.borrow() {
                cancel_requested
                    .changed()
                    .await
                    .context("turn cancellation channel closed")?;
            }
            if cancellation.borrow().is_none() {
                cancellation
                    .changed()
                    .await
                    .context("turn cancellation channel closed")?;
            }
            response = match cancellation
                .borrow()
                .clone()
                .expect("cancellation completed")
            {
                Ok(()) => Ok(ControllerResponse::ThreadCancelled),
                Err(message) => Err(anyhow!(message)),
            };
        }
        if !active.is_cancelling() {
            self.turns.finish(thread_id, &active).await;
        }
        response
    }
    pub(super) async fn run_turn(
        &self,
        thread_id: ThreadId,
        message: String,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ControllerResponse> {
        let _guard = self.thread_lock(thread_id).lock_owned().await;
        self.prepare_thread_for_turn(thread_id, updates).await?;
        self.sync_skills(thread_id, updates).await?;
        self.sync_runners(thread_id, updates).await?;
        self.store
            .name_thread_if_unnamed(thread_id, message.clone())
            .await
            .context("failed to name thread")?;
        self.sync_workspace_instructions(thread_id).await?;
        self.append_event(
            thread_id,
            ThreadEventData::UserMessage(MessageEvent { content: message }),
            updates,
        )
        .await
        .context("failed to save user message")?;
        self.continue_turn(thread_id, updates).await
    }

    pub(super) async fn continue_thread(
        &self,
        thread_id: ThreadId,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ControllerResponse> {
        let _guard = self.thread_lock(thread_id).lock_owned().await;
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
                if message.phase == Some(AssistantMessagePhase::Commentary) => {}
            Some(ThreadEventData::AssistantMessage(_)) => bail!("thread turn is already complete"),
            Some(ThreadEventData::ToolCall(_)) => unreachable!(),
            None => bail!("thread has no resumable history"),
            _ => unreachable!(),
        }
        self.sync_workspace_instructions(thread_id).await?;
        self.continue_turn(thread_id, updates).await
    }

    pub(super) async fn prepare_thread_for_turn(
        &self,
        thread_id: ThreadId,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<()> {
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load thread history")?;
        let Some(tool_call) = events
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.data,
                    ThreadEventData::UserMessage(_)
                        | ThreadEventData::AssistantMessage(_)
                        | ThreadEventData::ToolCall(_)
                        | ThreadEventData::ToolResult(_)
                        | ThreadEventData::Compaction(_)
                )
            })
            .filter(|event| matches!(event.data, ThreadEventData::ToolCall(_)))
        else {
            return Ok(());
        };
        self.turns.clear_approvals(thread_id).await;
        let ThreadEventData::ToolCall(call) = &tool_call.data else {
            unreachable!()
        };
        let (name, call_id, custom) = match call {
            ToolCallEvent::Custom { name, call_id, .. } => {
                (name.as_str(), Some(call_id.as_str()), true)
            }
            ToolCallEvent::Function { name, call_id, .. } => {
                (name.as_str(), call_id.as_deref(), false)
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
        .context("failed to save interrupted tool result")
    }

    pub(super) async fn cancel_thread(&self, thread_id: ThreadId) -> Result<ControllerResponse> {
        let Some(active) = self.turns.begin_cancellation(thread_id).await else {
            return Ok(ControllerResponse::ThreadNotActive);
        };
        let stop = active.request_cancellation().await;
        let cleanup = async {
            let _guard = self.thread_lock(thread_id).lock_owned().await;
            self.turns.clear_approvals(thread_id).await;
            self.prepare_thread_for_turn(thread_id, None).await
        }
        .await;
        let result = match (stop, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(stop), Ok(())) => Err(stop),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Err(stop), Err(cleanup)) => {
                Err(stop.context(format!("turn cleanup also failed: {cleanup:#}")))
            }
        };
        self.turns.finish(thread_id, &active).await;
        let outcome = result.map_err(|error| format!("{error:#}"));
        active.complete_cancellation(outcome.clone());
        outcome
            .map(|()| ControllerResponse::ThreadCancelled)
            .map_err(anyhow::Error::msg)
    }

    pub(super) fn thread_lock(&self, thread_id: ThreadId) -> Arc<Mutex<()>> {
        Arc::clone(
            self.thread_locks
                .lock()
                .unwrap()
                .entry(thread_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    pub(super) async fn ensure_no_pending_approval(&self, thread_id: ThreadId) -> Result<()> {
        self.turns.ensure_no_pending_approval(thread_id).await
    }

    pub(super) async fn continue_turn(
        &self,
        thread_id: ThreadId,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ControllerResponse> {
        let prompt_cache_key = format!(
            "{:x}",
            Sha256::digest(format!("{}-{thread_id}", self.prompt_cache_namespace))
        );
        let model_session = self.provider.start_turn(&prompt_cache_key).await?;
        loop {
            self.sync_runners(thread_id, updates).await?;
            let mut events = self
                .store
                .events(thread_id)
                .await
                .context("failed to load model history")?;
            let (model, reasoning_effort) = self
                .store
                .thread_model(thread_id)
                .await
                .context("failed to load thread model")?;
            let selected_model = self
                .provider
                .models()
                .await?
                .into_iter()
                .find(|candidate| candidate.id == model);
            let context_window = selected_model
                .as_ref()
                .and_then(|model| model.context_window);
            let auto_compact_token_limit =
                selected_model.and_then(|model| model.auto_compact_token_limit);
            let masked_tokens = self
                .mask_old_command_results(thread_id, &mut events, updates)
                .await?;
            let masked_tokens = i64::try_from(masked_tokens).unwrap_or(i64::MAX);
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
                .and_then(|usage| usage["total_tokens"].as_i64())
                .map(|tokens| tokens.saturating_sub(masked_tokens));
            if active_tokens
                .zip(auto_compact_token_limit)
                .is_some_and(|(tokens, limit)| tokens >= limit)
            {
                let request = self.provider.compaction_snapshot(
                    &model,
                    &reasoning_effort,
                    &events,
                    &prompt_cache_key,
                )?;
                self.append_event(
                    thread_id,
                    ThreadEventData::ModelRequest(ModelRequestEvent {
                        kind: ModelRequestKind::Compaction,
                        started_at_ms: unix_time_ms(),
                        request,
                        context_window,
                        auto_compact_token_limit,
                        compacted: events
                            .iter()
                            .any(|event| matches!(event.data, ThreadEventData::Compaction(_))),
                    }),
                    updates,
                )
                .await
                .context("failed to save compaction request")?;
                self.store
                    .create_checkpoint(thread_id, checkpoint_time_ms(), "compaction".to_owned())
                    .await
                    .context("failed to checkpoint history before compaction")?;
                let items = model_session
                    .compact(&model, &reasoning_effort, &events, &prompt_cache_key)
                    .await?;
                if !items.is_empty() {
                    let workspace_instructions = workspace_instructions(&events);
                    let workspace_event = match workspace_instructions {
                        WorkspaceInstructions::Untracked => None,
                        WorkspaceInstructions::Present(content) => {
                            Some(InstructionEvent::Initial(content))
                        }
                        WorkspaceInstructions::Removed => Some(InstructionEvent::Removal),
                    };
                    self.store
                        .replace_with_compaction(
                            thread_id,
                            CompactionEvent {
                                items: serde_json::to_value(items)
                                    .map_err(|error| anyhow!(error))?,
                            },
                            workspace_event,
                            skill_event(&events),
                            runner_event(&events),
                        )
                        .await
                        .context("failed to replace history after compaction")?;
                    events = self
                        .store
                        .events(thread_id)
                        .await
                        .context("failed to reload compacted model history")?;
                }
            }
            let request = self.provider.completion_snapshot(
                &model,
                &reasoning_effort,
                &events,
                &prompt_cache_key,
            )?;
            let request_sequence = self
                .append_event(
                    thread_id,
                    ThreadEventData::ModelRequest(ModelRequestEvent {
                        kind: ModelRequestKind::Response,
                        started_at_ms: unix_time_ms(),
                        request,
                        context_window,
                        auto_compact_token_limit,
                        compacted: events
                            .iter()
                            .any(|event| matches!(event.data, ThreadEventData::Compaction(_))),
                    }),
                    updates,
                )
                .await
                .context("failed to save model request")?;
            let completion = model_session
                .complete(
                    &model,
                    &reasoning_effort,
                    &events,
                    updates,
                    &prompt_cache_key,
                )
                .await?;
            for item in completion.reasoning {
                self.store
                    .append(
                        thread_id,
                        ThreadEventData::Reasoning(ItemEvent {
                            item: serde_json::to_value(item).map_err(|error| anyhow!(error))?,
                        }),
                    )
                    .await
                    .context("failed to save encrypted reasoning")?;
            }
            if let Some(usage) = completion.token_usage {
                self.append_event(
                    thread_id,
                    ThreadEventData::TokenUsage(TokenUsageEvent {
                        request_sequence,
                        usage: serde_json::to_value(usage).map_err(|error| anyhow!(error))?,
                    }),
                    updates,
                )
                .await
                .context("failed to save token usage")?;
            }
            if !completion.rate_limits.is_empty() {
                self.append_event(
                    thread_id,
                    ThreadEventData::RateLimits(RateLimitsEvent {
                        request_sequence,
                        snapshots: serde_json::to_value(completion.rate_limits)
                            .map_err(|error| anyhow!(error))?,
                    }),
                    updates,
                )
                .await
                .context("failed to save rate limits")?;
            }

            if let Some(response) = self
                .execute_model_responses(thread_id, completion.responses.into(), updates)
                .await?
            {
                return Ok(response);
            }
        }
    }

    pub(super) async fn mask_old_command_results(
        &self,
        thread_id: ThreadId,
        events: &mut Vec<storage::Event>,
        stream_updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<usize> {
        let previous_boundary = storage::latest_frozen_boundary(events);
        let active_through = previous_boundary
            .as_ref()
            .map(|boundary| boundary.through_sequence)
            .into_iter()
            .chain(
                events
                    .iter()
                    .rfind(|event| matches!(event.data, ThreadEventData::Compaction(_)))
                    .map(|event| event.sequence),
            )
            .max();
        let active_start = active_through.map_or(0, |sequence| {
            events.partition_point(|event| event.sequence <= sequence)
        });
        if self.provider.context_tokens(&events[active_start..])? <= ACTIVE_CONTEXT_HIGH_TOKENS {
            return Ok(0);
        }
        let context_tokens_before = self.provider.context_tokens(events)?;
        let mut suffix_start = active_start;
        let mut suffix_end = events.len();
        while suffix_start < suffix_end {
            let middle = suffix_start + (suffix_end - suffix_start) / 2;
            if self.provider.context_tokens(&events[middle..])? <= ACTIVE_CONTEXT_LOW_TOKENS {
                suffix_end = middle;
            } else {
                suffix_start = middle + 1;
            }
        }
        let freeze_through_index = suffix_start.saturating_sub(1);

        let request_sequences = events
            .iter()
            .filter(|event| {
                matches!(&event.data, ThreadEventData::ModelRequest(request) if request.kind == ModelRequestKind::Response)
            })
            .map(|event| event.sequence)
            .collect::<Vec<_>>();
        let mut masked_events = Vec::new();
        let mut through_sequence = None;
        for (index, event) in events.iter().enumerate().skip(active_start) {
            let later_requests =
                request_sequences.partition_point(|sequence| *sequence <= event.sequence);
            let ThreadEventData::ToolResult(result) = &event.data else {
                continue;
            };
            if request_sequences.len() - later_requests < MINIMUM_FULL_RESULT_REQUESTS {
                continue;
            }
            through_sequence = Some(event.sequence);
            if let Some(masked_result) = masked_tool_result(result) {
                let mut event = event.clone();
                match &mut event.data {
                    ThreadEventData::ToolResult(ToolResultEvent::Custom {
                        masked_result: field,
                        ..
                    })
                    | ThreadEventData::ToolResult(ToolResultEvent::Function {
                        masked_result: field,
                        ..
                    }) => *field = Some(serde_json::Value::String(masked_result)),
                    _ => unreachable!(),
                }
                masked_events.push((index, event));
            }
            if index >= freeze_through_index {
                break;
            }
        }
        if masked_events.is_empty() {
            return Ok(0);
        }
        let through_sequence = through_sequence.expect("a masked event was passed");
        let mut masked_sequences = previous_boundary
            .map(|boundary| boundary.masked_sequences)
            .unwrap_or_default();
        masked_sequences.extend(masked_events.iter().map(|(_, event)| event.sequence));
        let boundary_data = FrozenBoundaryEvent {
            through_sequence,
            masked_sequences,
        };
        let mut projected_events = events.clone();
        for (index, event) in &masked_events {
            projected_events[*index] = event.clone();
        }
        projected_events.push(storage::Event {
            sequence: events.last().map_or(EventSequence(0), |event| {
                EventSequence(event.sequence.0 + 1)
            }),
            data: ThreadEventData::FrozenBoundary(boundary_data.clone()),
        });
        let masked_tokens =
            context_tokens_before.saturating_sub(self.provider.context_tokens(&projected_events)?);
        if masked_tokens == 0 {
            return Ok(0);
        }
        let sequence = self
            .store
            .freeze_event_payloads(
                thread_id,
                masked_events
                    .iter()
                    .map(|(_, event)| (event.sequence, event.data.clone()))
                    .collect(),
                boundary_data.clone(),
            )
            .await
            .context("failed to mask old command results")?;
        for (index, event) in &masked_events {
            events[*index] = event.clone();
        }
        let boundary = storage::Event {
            sequence,
            data: ThreadEventData::FrozenBoundary(boundary_data),
        };
        events.push(boundary.clone());
        if let Some(stream_updates) = stream_updates {
            let _ = stream_updates.send(ModelStreamEvent::ThreadEvent(protocol_event(boundary)));
            for (_, event) in &masked_events {
                let _ = stream_updates
                    .send(ModelStreamEvent::ThreadEvent(protocol_event(event.clone())));
            }
        }
        Ok(masked_tokens)
    }

    pub(super) async fn sync_workspace_instructions(&self, thread_id: ThreadId) -> Result<()> {
        let content = self.read_workspace_instructions().await?;
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load workspace instruction state")?;
        let previous = workspace_instructions(&events);
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
        self.store
            .append(thread_id, ThreadEventData::WorkspaceInstructions(event))
            .await
            .context("failed to save workspace instructions")?;
        Ok(())
    }

    pub(super) async fn sync_skills(
        &self,
        thread_id: ThreadId,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<()> {
        let generation = self.collect_skill_generation().await?;

        self.runners
            .sync_skills(&self.skill_store, &generation)
            .await?;
        *self.skill_generation.lock().await = Some(Arc::clone(&generation));

        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load skill state")?;
        let previous = current_skills(&events);
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
            return Ok(());
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
        Ok(())
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
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<()> {
        let runners = self.runners.list().await?;
        let events = self
            .store
            .events(thread_id)
            .await
            .context("failed to load runner state")?;
        let previous = current_runners(&events);
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
        thread_id: ThreadId,
        mut responses: VecDeque<ModelResponse>,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<Option<ControllerResponse>> {
        let mut final_answer = None;
        let mut needs_follow_up = false;
        while let Some(response) = responses.pop_front() {
            match response {
                ModelResponse::AssistantMessage { content, phase } => {
                    self.append_event(
                        thread_id,
                        ThreadEventData::AssistantMessage(AssistantMessageEvent {
                            content: content.clone(),
                            phase,
                        }),
                        updates,
                    )
                    .await
                    .context("failed to save assistant message")?;
                    if phase != Some(AssistantMessagePhase::Commentary) {
                        final_answer = Some(content);
                    }
                }
                ModelResponse::WebSearch { item } => {
                    self.append_event(
                        thread_id,
                        ThreadEventData::WebSearch(ItemEvent {
                            item: serde_json::to_value(item).map_err(|error| anyhow!(error))?,
                        }),
                        updates,
                    )
                    .await
                    .context("failed to save web search")?;
                }
                ModelResponse::ToolCall {
                    name,
                    arguments,
                    call_id,
                } => {
                    needs_follow_up = true;
                    self.append_event(
                        thread_id,
                        ThreadEventData::ToolCall(ToolCallEvent::Function {
                            name: name.clone(),
                            arguments: arguments.clone(),
                            call_id: call_id.clone(),
                        }),
                        updates,
                    )
                    .await
                    .context("failed to save tool call")?;
                    match name.as_str() {
                        "exec_command" => {
                            let arguments: ExecCommandArguments = serde_json::from_value(arguments)
                                .context("fake model returned invalid exec_command arguments")?;
                            if let Some(response) = self
                                .route_tool(
                                    thread_id,
                                    name,
                                    call_id,
                                    ToolArguments::ExecCommand(arguments),
                                    false,
                                    updates,
                                )
                                .await?
                            {
                                return Ok(Some(response));
                            }
                        }
                        "apply_patch" => {
                            let arguments: ApplyPatchArguments = serde_json::from_value(arguments)
                                .context("fake model returned invalid apply_patch arguments")?;
                            if let Some(response) = self
                                .route_tool(
                                    thread_id,
                                    name,
                                    call_id,
                                    ToolArguments::ApplyPatch(arguments),
                                    false,
                                    updates,
                                )
                                .await?
                            {
                                return Ok(Some(response));
                            }
                        }
                        "wait_process" => {
                            let arguments: WaitProcessArguments = serde_json::from_value(arguments)
                                .context("model returned invalid wait_process arguments")?;
                            if let Some(response) = self
                                .route_tool(
                                    thread_id,
                                    name,
                                    call_id,
                                    ToolArguments::WaitProcess(arguments),
                                    false,
                                    updates,
                                )
                                .await?
                            {
                                return Ok(Some(response));
                            }
                        }
                        "stop_process" => {
                            let arguments: StopProcessArguments = serde_json::from_value(arguments)
                                .context("model returned invalid stop_process arguments")?;
                            if let Some(response) = self
                                .route_tool(
                                    thread_id,
                                    name,
                                    call_id,
                                    ToolArguments::StopProcess(arguments),
                                    false,
                                    updates,
                                )
                                .await?
                            {
                                return Ok(Some(response));
                            }
                        }
                        _ => bail!("model requested unsupported tool {name}"),
                    }
                }
                ModelResponse::CustomToolCall {
                    item_id,
                    name,
                    input,
                    call_id,
                } => {
                    needs_follow_up = true;
                    if name != "runner" {
                        bail!("model requested unsupported custom tool {name}");
                    }
                    self.append_event(
                        thread_id,
                        ThreadEventData::ToolCall(ToolCallEvent::Custom {
                            call_type: CustomToolType::Custom,
                            item_id: item_id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            call_id: call_id.clone(),
                        }),
                        updates,
                    )
                    .await
                    .context("failed to save tool call")?;
                    let mut results = Vec::new();
                    let mut artifacts = Vec::new();
                    for (index, operation) in parse_runner_input(&input)?.into_iter().enumerate() {
                        let operation_index = index + 1;
                        let runner = operation.runner().to_owned();
                        let operation_name = operation.name();
                        let result_label = operation.result_label();
                        let operation_context = OperationContext {
                            call_id: call_id.clone(),
                            index: operation_index,
                            label: result_label.clone(),
                        };
                        let outcome = self
                            .approve_and_execute(
                                thread_id,
                                operation_name,
                                operation.into_arguments(),
                                Some(&operation_context),
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
                            runner,
                            label: result_label,
                            result: serde_json::Value::String(result),
                            artifacts: outcome.artifacts,
                        });
                        send_operation_update(
                            Some(&operation_context),
                            updates,
                            RunnerOperationUpdate::Completed {
                                artifact: artifact.clone(),
                            },
                        )?;
                        artifacts.push(artifact);
                    }
                    self.save_tool_result(
                        thread_id,
                        &name,
                        Some(&call_id),
                        ToolOutcome {
                            result: serde_json::Value::String(results.join("\n\n")),
                            artifacts,
                        },
                        true,
                        updates,
                    )
                    .await?;
                }
            }
        }
        Ok((!needs_follow_up)
            .then_some(final_answer)
            .flatten()
            .map(|content| ControllerResponse::TurnCompleted { content }))
    }

    pub(super) async fn route_tool(
        &self,
        thread_id: ThreadId,
        name: String,
        call_id: Option<String>,
        arguments: ToolArguments,
        custom: bool,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<Option<ControllerResponse>> {
        let result = self
            .approve_and_execute(thread_id, &name, arguments, None, updates)
            .await?;
        self.save_tool_result(
            thread_id,
            &name,
            call_id.as_deref(),
            result,
            custom,
            updates,
        )
        .await?;
        Ok(None)
    }

    pub(super) async fn approve_and_execute(
        &self,
        thread_id: ThreadId,
        name: &str,
        arguments: ToolArguments,
        operation: Option<&OperationContext>,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ToolOutcome> {
        let runner = self.runners.get(arguments.runner()).await?;
        let decision =
            if arguments.requires_approval() && runner.approval().await == ApprovalPolicy::Ask {
                let arguments_json = serde_json::to_value(&arguments)
                    .context("failed to encode approval arguments")?;
                let (approval_id, approval) = self.turns.register_approval(thread_id).await?;
                updates
                    .context("approval requires a streaming turn")?
                    .send(ModelStreamEvent::ApprovalRequired {
                        approval_id,
                        thread_id,
                        tool: name.to_owned(),
                        arguments: arguments_json,
                        operation_index: operation.map(|operation| operation.index),
                        operation_label: operation.map(|operation| operation.label.clone()),
                    })
                    .context("turn stream closed while waiting for approval")?;
                Some(
                    approval
                        .await
                        .context("approval was removed before it was resolved")?,
                )
            } else {
                None
            };
        let allowed = decision.as_ref().is_none_or(|decision| decision.allowed);
        let active = if allowed && matches!(&arguments, ToolArguments::ApplyPatch(_)) {
            Some(
                self.turns
                    .get(thread_id)
                    .await
                    .context("thread has no active turn")?,
            )
        } else {
            None
        };
        let _uncancellable = match &active {
            Some(active) => Some(active.lock_uncancellable().await),
            None => None,
        };
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

    pub(super) async fn resolve_approval(
        &self,
        approval_id: ApprovalId,
        allowed: bool,
        reason: Option<String>,
    ) -> Result<ControllerResponse> {
        self.turns
            .resolve_approval(approval_id, ApprovalDecision { allowed, reason })
            .await?;
        Ok(ControllerResponse::ApprovalResolved)
    }

    pub(super) async fn execute(
        &self,
        thread_id: ThreadId,
        arguments: ToolArguments,
        operation: Option<&OperationContext>,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<ToolOutcome> {
        match arguments {
            ToolArguments::ExecCommand(arguments) => {
                let runner_name = arguments.runner.clone();
                let command = arguments.command.clone();
                let started_at_ms = checkpoint_time_ms();
                let runner = self.runners.get(&arguments.runner).await?;
                if let ModelCommandMode::Background { process_id } = &arguments.mode
                    && self
                        .runners
                        .contains_process(&ProcessKey {
                            thread_id,
                            runner: runner_name.clone(),
                            process_id: process_id.clone(),
                        })
                        .await
                {
                    return Ok(ToolOutcome::text(format!(
                        "Process ID '{process_id}' is already in use on Runner {runner_name}"
                    )));
                }
                let response = match arguments.mode {
                    ModelCommandMode::Background { process_id } => {
                        let process_handle = runner.start_command(arguments.command).await?;
                        self.runners
                            .insert_process(
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
                            )
                            .await;
                        CommandOutcome::Started { process_id }
                    }
                    ModelCommandMode::Foreground { timeout_ms } => {
                        let active = self
                            .turns
                            .get(thread_id)
                            .await
                            .context("thread has no active turn")?;
                        let process_handle = runner.start_command(arguments.command).await?;
                        active
                            .set_process(Arc::clone(&runner), process_handle.clone())
                            .await;
                        send_operation_update(
                            operation,
                            updates,
                            RunnerOperationUpdate::CommandStarted,
                        )?;
                        let deadline =
                            Instant::now() + std::time::Duration::from_millis(timeout_ms);
                        let mut collected = None;
                        let response = loop {
                            let timeout_ms = deadline
                                .saturating_duration_since(Instant::now())
                                .as_millis()
                                .min(1000)
                                .try_into()
                                .unwrap_or(1000);
                            match runner.wait(process_handle.clone(), timeout_ms).await? {
                                WaitOutcome::Running { output, .. } => {
                                    send_operation_update(
                                        operation,
                                        updates,
                                        RunnerOperationUpdate::CommandOutput {
                                            content: output.content.clone(),
                                            omitted_bytes: output.omitted_bytes,
                                        },
                                    )?;
                                    append_command_output(&mut collected, output);
                                    if Instant::now() >= deadline {
                                        break WaitOutcome::Running {
                                            process_handle: process_handle.clone(),
                                            output: collected.take().unwrap(),
                                        };
                                    }
                                }
                                WaitOutcome::Finished { output, exit_code } => {
                                    append_command_output(&mut collected, output);
                                    break WaitOutcome::Finished {
                                        output: collected.take().unwrap(),
                                        exit_code,
                                    };
                                }
                            }
                        };
                        active.clear_process().await;
                        match response {
                            WaitOutcome::Running {
                                process_handle,
                                output,
                            } => {
                                let process_id = self
                                    .runners
                                    .register_generated_process(
                                        thread_id,
                                        &runner_name,
                                        process_handle,
                                        command.clone(),
                                        started_at_ms,
                                    )
                                    .await;
                                CommandOutcome::Running { process_id, output }
                            }
                            WaitOutcome::Finished { output, exit_code } => {
                                CommandOutcome::Finished { output, exit_code }
                            }
                        }
                    }
                };
                let artifact = command_artifact(&response, &runner_name);
                Ok(ToolOutcome::with_artifact(
                    format_command_response(&runner_name, response),
                    ToolArtifact::CommandExecution(artifact),
                ))
            }
            ToolArguments::ApplyPatch(arguments) => {
                let result = self
                    .runners
                    .get(&arguments.runner)
                    .await?
                    .client
                    .apply_patch(arguments.patch)
                    .await?;
                let output = format_patch_result(&result);
                Ok(ToolOutcome::with_artifact(
                    output,
                    ToolArtifact::PatchOperations(result),
                ))
            }
            ToolArguments::WaitProcess(arguments) => {
                let runner_name = arguments.runner.clone();
                let process_handle = self
                    .runners
                    .process(&ProcessKey {
                        thread_id,
                        runner: runner_name.clone(),
                        process_id: arguments.process_id.clone(),
                    })
                    .await
                    .map(|process| process.handle.clone());
                let Some(process_handle) = process_handle else {
                    return Ok(ToolOutcome::text(format!(
                        "Process ID '{}' is not running on Runner {runner_name}",
                        arguments.process_id
                    )));
                };
                let runner = self.runners.get(&arguments.runner).await?;
                send_operation_update(operation, updates, RunnerOperationUpdate::WaitStarted)?;
                let deadline =
                    Instant::now() + std::time::Duration::from_millis(arguments.timeout_ms);
                let mut collected = None;
                let response = loop {
                    let timeout_ms = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(1000)
                        .try_into()
                        .unwrap_or(1000);
                    match runner.wait(process_handle.clone(), timeout_ms).await? {
                        WaitOutcome::Running { output, .. } => {
                            send_operation_update(
                                operation,
                                updates,
                                RunnerOperationUpdate::CommandOutput {
                                    content: output.content.clone(),
                                    omitted_bytes: output.omitted_bytes,
                                },
                            )?;
                            append_command_output(&mut collected, output);
                            if Instant::now() >= deadline {
                                break CommandOutcome::Running {
                                    process_id: arguments.process_id.clone(),
                                    output: collected.take().unwrap(),
                                };
                            }
                        }
                        WaitOutcome::Finished { output, exit_code } => {
                            append_command_output(&mut collected, output);
                            break CommandOutcome::Finished {
                                output: collected.take().unwrap(),
                                exit_code,
                            };
                        }
                    }
                };
                let response = match response {
                    CommandOutcome::Running { output, .. } => CommandOutcome::Running {
                        process_id: arguments.process_id.clone(),
                        output,
                    },
                    response @ CommandOutcome::Finished { .. } => {
                        self.runners
                            .remove_process(&ProcessKey {
                                thread_id,
                                runner: runner_name.clone(),
                                process_id: arguments.process_id.clone(),
                            })
                            .await;
                        response
                    }
                    response => response,
                };
                let artifact = command_artifact(&response, &runner_name);
                Ok(ToolOutcome::with_artifact(
                    format_command_response(&runner_name, response),
                    ToolArtifact::CommandExecution(artifact),
                ))
            }
            ToolArguments::StopProcess(arguments) => {
                let runner_name = arguments.runner.clone();
                let process_handle = self
                    .runners
                    .process(&ProcessKey {
                        thread_id,
                        runner: runner_name.clone(),
                        process_id: arguments.process_id.clone(),
                    })
                    .await
                    .map(|process| process.handle.clone());
                let Some(process_handle) = process_handle else {
                    return Ok(ToolOutcome::text(format!(
                        "Process ID '{}' is not running on Runner {runner_name}",
                        arguments.process_id
                    )));
                };
                let response = self
                    .runners
                    .get(&arguments.runner)
                    .await?
                    .stop(process_handle)
                    .await?;
                self.runners
                    .remove_process(&ProcessKey {
                        thread_id,
                        runner: runner_name.clone(),
                        process_id: arguments.process_id,
                    })
                    .await;
                let response = CommandOutcome::Stopped { output: response };
                let artifact = command_artifact(&response, &runner_name);
                Ok(ToolOutcome::with_artifact(
                    format_command_response(&runner_name, response),
                    ToolArtifact::CommandExecution(artifact),
                ))
            }
        }
    }

    pub(super) async fn save_tool_result(
        &self,
        thread_id: ThreadId,
        name: &str,
        call_id: Option<&str>,
        outcome: ToolOutcome,
        custom: bool,
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> Result<()> {
        let data = if custom {
            ThreadEventData::ToolResult(ToolResultEvent::Custom {
                call_type: CustomToolType::Custom,
                name: name.to_owned(),
                call_id: call_id.map(str::to_owned),
                result: outcome.result,
                artifacts: outcome.artifacts,
                masked_result: None,
            })
        } else {
            ThreadEventData::ToolResult(ToolResultEvent::Function {
                call_type: None,
                name: name.to_owned(),
                call_id: call_id.map(str::to_owned),
                result: outcome.result,
                artifacts: outcome.artifacts,
                masked_result: None,
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
        updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    ) -> tokio_rusqlite::Result<EventSequence> {
        let sequence = self.store.append(thread_id, data.clone()).await?;
        if let Some(updates) = updates {
            updates
                .send(ModelStreamEvent::ThreadEvent(ThreadEvent {
                    sequence,
                    data,
                }))
                .ok();
        }
        Ok(sequence)
    }
}
