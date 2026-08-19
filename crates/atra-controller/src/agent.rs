use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_protocol::{
    AgentRequest, AgentResponse, AgentTarget, AssistantMessagePhase, EventSequence,
    ThreadEventData, ThreadId, TurnOutcome, TurnPhase,
};
use futures_util::future::join_all;
use tokio::time::Instant;

use crate::{State, lifecycle::ActiveTurn};

pub(super) async fn run_callback_operation<T, Accepted, Operation>(
    name: &'static str,
    acceptance: Accepted,
) -> Result<T>
where
    Accepted: Future<Output = Result<Operation>>,
    Operation: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    // Validation and reservation happen in the callback-owned future. The
    // returned continuation is the explicit acceptance boundary.
    let operation = acceptance.await?;
    tokio::spawn(operation)
        .await
        .with_context(|| format!("{name} operation task failed"))
}

impl State {
    pub(super) async fn handle_agent_request(
        self: &Arc<Self>,
        execution_context: &str,
        request: AgentRequest,
    ) -> AgentResponse {
        let result = async {
            let invoking = self
                .execution_contexts
                .lock()
                .unwrap()
                .get(execution_context)
                .copied()
                .context("agent execution context has expired")?;
            self.agent_request(invoking, request).await
        }
        .await;
        match result {
            Ok((output, success)) => AgentResponse { output, success },
            Err(error) => AgentResponse {
                output: format!("{error:#}"),
                success: false,
            },
        }
    }

    async fn agent_request(
        self: &Arc<Self>,
        invoking: ThreadId,
        request: AgentRequest,
    ) -> Result<(String, bool)> {
        self.materialize_thread(invoking).await?;
        match request {
            AgentRequest::Create {
                name,
                model,
                effort,
                allow_delegation,
            } => {
                if name.trim().is_empty() {
                    bail!("agent name must not be empty");
                }
                let parent = self.store.thread(invoking).await?;
                if parent.parent_thread_id.is_some()
                    && !self.store.delegation_allowed(invoking).await?
                {
                    bail!("this subagent is not allowed to create child agents");
                }
                let (provider, model_id) = match model {
                    Some(profile) => profile
                        .split_once('/')
                        .map(|(p, m)| (p.to_owned(), m.to_owned()))
                        .context("model must be provider/model")?,
                    None => (parent.provider.clone(), parent.model.clone()),
                };
                let models = self.provider(&provider)?.models().await?;
                let selected = models
                    .iter()
                    .find(|candidate| candidate.id == model_id)
                    .with_context(|| format!("unknown model {provider}/{model_id}"))?;
                let reasoning_effort = effort.unwrap_or(parent.reasoning_effort);
                if !selected
                    .supported_reasoning_efforts
                    .iter()
                    .any(|candidate| candidate == &reasoning_effort)
                {
                    bail!(
                        "reasoning effort {reasoning_effort} is not supported by model {provider}/{model_id}"
                    );
                }
                let state = Arc::clone(self);
                let id = run_callback_operation("agent create", async move {
                    let mutation = state.lock_mutation().await?;
                    state.store.thread(invoking).await?;
                    Ok(async move {
                        let _mutation = mutation;
                        let id = state
                            .store
                            .create_child_thread(
                                invoking,
                                name,
                                provider,
                                model_id,
                                reasoning_effort,
                                allow_delegation,
                            )
                            .await?;
                        let thread = state.store.thread(id).await?;
                        state.views.add_thread(thread).await?;
                        Ok::<_, anyhow::Error>(id)
                    })
                })
                .await??;
                Ok((format!("thread_id={id}"), true))
            }
            AgentRequest::Send { thread_id, message } => {
                let (_, after) = self.start_agent_turn(invoking, thread_id, message).await?;
                Ok((format!("after_sequence={after}"), true))
            }
            AgentRequest::Wait {
                targets,
                timeout_ms,
            } => self.agent_wait(invoking, targets, timeout_ms).await,
            AgentRequest::List => {
                let invoking_thread = self.store.thread(invoking).await?;
                let descendants = self.store.descendants(invoking).await?;
                let mut descendant_threads = Vec::with_capacity(descendants.len());
                for id in descendants {
                    descendant_threads.push(self.store.thread(id).await?);
                }
                let descendants = recursive_preorder(invoking, descendant_threads)?;
                let mut rows = Vec::with_capacity(descendants.len() + 1);
                rows.push(self.agent_list_row(&invoking_thread, 0).await?);
                for (thread, depth) in descendants {
                    rows.push(self.agent_list_row(&thread, depth).await?);
                }
                Ok((rows.join("\n"), true))
            }
            AgentRequest::Cancel {
                thread_ids,
                recursive,
            } => {
                let state = Arc::clone(self);
                run_callback_operation("agent cancel", async move {
                    state
                        .accept_agent_cancellation(invoking, thread_ids, recursive)
                        .await
                })
                .await
            }
            AgentRequest::Delete {
                thread_ids,
                recursive,
            } => {
                let state = Arc::clone(self);
                run_callback_operation("agent delete", async move {
                    state
                        .accept_agent_deletion(invoking, thread_ids, recursive)
                        .await
                })
                .await?
            }
        }
    }

    async fn accept_agent_cancellation(
        self: &Arc<Self>,
        invoking: ThreadId,
        thread_ids: Vec<ThreadId>,
        recursive: bool,
    ) -> Result<impl Future<Output = (String, bool)> + Send + 'static + use<>> {
        let mut success = true;
        let mut rows = Vec::new();
        let mut covered = HashSet::new();
        let (mutation, entries) = {
            let mutation = self.lock_mutation().await?;
            let mut entries = Vec::new();
            let mut ordered = Vec::new();
            for requested in normalize_requested_ids(thread_ids) {
                if covered.contains(&requested) {
                    continue;
                }
                let snapshot = async {
                    self.authorize_descendant(invoking, requested).await?;
                    let mut ids = vec![requested];
                    if recursive {
                        ids.extend(self.store.descendants(requested).await?);
                    }
                    Ok::<_, anyhow::Error>(ids)
                }
                .await;
                let snapshot = match snapshot {
                    Ok(ids) => {
                        let ids = ids
                            .into_iter()
                            .filter(|id| covered.insert(*id))
                            .collect::<Vec<_>>();
                        ordered.extend(ids.iter().copied());
                        Ok(ids)
                    }
                    Err(error) => {
                        success = false;
                        Err(format!("{error:#}"))
                    }
                };
                entries.push((requested, snapshot));
            }
            self.turns.begin_cancellation_many(&ordered);
            (mutation, entries)
        };
        Ok(async move {
            drop(mutation);
            for (requested, snapshot) in entries {
                match snapshot {
                    Ok(ids) => {
                        for id in ids {
                            rows.push(format!("thread={id} status=accepted"));
                        }
                    }
                    Err(error) => rows.push(format!("thread={requested} error={error}")),
                }
            }
            (rows.join("\n"), success)
        })
    }

    async fn accept_agent_deletion(
        self: &Arc<Self>,
        invoking: ThreadId,
        thread_ids: Vec<ThreadId>,
        recursive: bool,
    ) -> Result<impl Future<Output = Result<(String, bool)>> + Send + 'static + use<>> {
        let mut success = true;
        let mut rows = Vec::new();
        let mutation = self.lock_mutation().await?;
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for id in thread_ids {
            if seen.insert(id) {
                unique.push(id);
            }
        }
        let mut snapshots = HashMap::new();
        let mut validation_errors = HashMap::new();
        for id in &unique {
            match self.authorize_descendant(invoking, *id).await {
                Ok(()) => {
                    let mut descendants = self.store.descendants(*id).await?;
                    if !recursive && !descendants.is_empty() {
                        validation_errors.insert(
                            *id,
                            "cannot delete a thread with descendants without recursive deletion"
                                .to_owned(),
                        );
                    } else {
                        descendants.push(*id);
                        snapshots.insert(*id, descendants);
                    }
                }
                Err(error) => {
                    validation_errors.insert(*id, format!("{error:#}"));
                }
            }
        }
        let roots = normalized_delete_roots(&snapshots);
        let mut outcomes = HashMap::new();
        let mut accepted = Vec::new();
        for root in roots {
            let mut subtree = snapshots[&root].clone();
            subtree.sort_by_key(|id| std::cmp::Reverse(id.0));
            match self.turns.ensure_deletable(&subtree) {
                Ok(()) => accepted.push((root, subtree)),
                Err(error) => {
                    outcomes.insert(root, Err(format!("{error:#}")));
                }
            }
        }
        let state = Arc::clone(self);
        Ok(async move {
            let mut cleanups = Vec::new();
            for (root, subtree) in accepted {
                if let Err(error) = state.views.validate_delete_threads(&subtree).await {
                    outcomes.insert(root, Err(format!("{error:#}")));
                    continue;
                }
                let outcome = state
                    .commit_thread_snapshot(&subtree)
                    .await
                    .map_err(|error| format!("{error:#}"));
                if outcome.is_ok() {
                    cleanups.extend(state.runners.take_thread_processes(&subtree).await);
                }
                outcomes.insert(root, outcome);
            }
            drop(mutation);
            crate::runner_pool::RunnerPool::stop_taken_processes(cleanups).await;
            for id in unique {
                if let Some(error) = validation_errors.get(&id) {
                    success = false;
                    rows.push(format!("thread={id} error={error}"));
                    continue;
                }
                let root = outcomes
                    .keys()
                    .copied()
                    .find(|root| snapshots[root].contains(&id))
                    .context("delete target was not covered by its normalized snapshot")?;
                match &outcomes[&root] {
                    Ok(()) => rows.push(format!("thread={id} status=deleted")),
                    Err(error) => {
                        success = false;
                        rows.push(format!("thread={id} error={error}"));
                    }
                }
            }
            Ok((rows.join("\n"), success))
        })
    }

    pub(super) async fn delete_thread_subtree(
        &self,
        thread_id: ThreadId,
        recursive: bool,
    ) -> Result<Vec<ThreadId>> {
        let mutation = self.lock_mutation().await?;
        let mut subtree = self.store.descendants(thread_id).await?;
        if !recursive && !subtree.is_empty() {
            bail!("cannot delete a thread with descendants without recursive deletion");
        }
        subtree.push(thread_id);
        subtree.sort_by_key(|id| std::cmp::Reverse(id.0));

        self.turns.ensure_deletable(&subtree)?;
        self.views.validate_delete_threads(&subtree).await?;
        let result = self.commit_thread_snapshot(&subtree).await;
        let cleanup = if result.is_ok() {
            self.runners.take_thread_processes(&subtree).await
        } else {
            Vec::new()
        };
        drop(mutation);
        crate::runner_pool::RunnerPool::stop_taken_processes(cleanup).await;
        result?;
        Ok(subtree)
    }

    async fn commit_thread_snapshot(&self, subtree: &[ThreadId]) -> Result<()> {
        self.store.delete_threads(subtree.to_vec()).await?;
        self.views.delete_threads(subtree).await;
        Ok(())
    }

    async fn authorize_descendant(&self, invoking: ThreadId, target: ThreadId) -> Result<()> {
        if target == invoking || !self.store.is_descendant(invoking, target).await? {
            bail!("thread {target} is not a descendant of invoking thread {invoking}");
        }
        Ok(())
    }

    async fn agent_list_row(&self, thread: &atra_protocol::Thread, depth: usize) -> Result<String> {
        self.materialize_thread(thread.id).await?;
        let state = self
            .views
            .thread_state(thread.id)
            .await
            .context("thread state is not loaded")?;
        Ok(format!(
            "{}{} thread={} model={}/{} effort={} status={}",
            "  ".repeat(depth),
            thread_label(thread),
            thread.id,
            thread.provider,
            thread.model,
            thread.reasoning_effort,
            turn_status(&state)
        ))
    }

    async fn agent_wait(
        &self,
        invoking: ThreadId,
        targets: Vec<AgentTarget>,
        timeout_ms: u64,
    ) -> Result<(String, bool)> {
        if targets.is_empty() {
            bail!("at least one wait target is required");
        }
        let (pins, active) = {
            let _mutation = self.lock_mutation().await?;
            for target in &targets {
                self.authorize_descendant(invoking, target.thread_id)
                    .await?;
                self.materialize_thread_locked(target.thread_id).await?;
            }
            let ids = targets
                .iter()
                .map(|target| target.thread_id)
                .collect::<Vec<_>>();
            self.turns.pin_many(&ids)?
        };
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(timeout_ms))
            .context("timeout is too large")?;
        let waits =
            targets
                .into_iter()
                .zip(active)
                .zip(pins)
                .map(|((target, turn), pin)| async move {
                    let thread_id = target.thread_id;
                    let result = async {
                        if let Some(turn) = turn {
                            wait_for_turn(self, thread_id, &turn, deadline).await?;
                        }
                        self.agent_report(target).await
                    }
                    .await;
                    pin.release();
                    (thread_id, result)
                });
        let results = join_all(waits).await;
        let mut output = Vec::new();
        let mut success = true;
        for (thread_id, result) in results {
            match result {
                Ok((report, failed)) => {
                    output.push(report);
                    success &= !failed;
                }
                Err(error) => {
                    output.push(format!("thread={thread_id} error={error:#}"));
                    success = false;
                }
            }
        }
        Ok((output.join("\n"), success))
    }

    async fn agent_report(&self, target: AgentTarget) -> Result<(String, bool)> {
        let thread = self.store.thread(target.thread_id).await?;
        let state = self
            .views
            .thread_state(target.thread_id)
            .await
            .context("thread state is not loaded")?;
        let snapshot = self.store.report_snapshot(target.thread_id).await?;
        let through = snapshot.through;
        let status = turn_status(&state);
        if through.0 < 0 {
            return Ok((
                format!(
                    "== {} thread={} status={status} events=none through=-1 ==",
                    thread_label(&thread),
                    thread.id
                ),
                matches!(state.last_outcome(), Some(TurnOutcome::Failed { .. })),
            ));
        }
        let events = snapshot.events;
        let after = target.after_sequence;
        let selected = events
            .iter()
            .filter(|event| event.sequence.0 > after.0)
            .cloned()
            .collect::<Vec<_>>();
        let range = selected
            .first()
            .zip(selected.last())
            .map_or("none".to_owned(), |(first, last)| {
                format!("{}..{}", first.sequence, last.sequence)
            });
        let mut lines = vec![format!(
            "== {} thread={} status={status} events={range} through={through} ==",
            thread_label(&thread),
            thread.id
        )];
        lines.extend(render_report_events(&selected, &events));
        Ok((
            lines.join("\n"),
            matches!(state.last_outcome(), Some(TurnOutcome::Failed { .. })),
        ))
    }
}

async fn wait_for_turn(
    state: &State,
    thread_id: ThreadId,
    turn: &Arc<ActiveTurn>,
    deadline: Instant,
) -> Result<()> {
    let mut finished = turn.finished();
    loop {
        if *finished.borrow() {
            return Ok(());
        }
        if let Some(public) = state.views.thread_state(thread_id).await
            && public
                .active_turn()
                .is_some_and(|active| active.pending_question().is_some())
        {
            return Ok(());
        }
        tokio::select! {
            changed = finished.changed() => { if changed.is_err() || *finished.borrow() { return Ok(()); } }
            () = tokio::time::sleep_until(deadline) => return Ok(()),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}

fn turn_status(state: &atra_protocol::ThreadState) -> &'static str {
    if let Some(turn) = state.active_turn() {
        if turn.pending_question().is_some() {
            return "awaiting_question";
        }
        if turn.pending_approval().is_some() {
            return "awaiting_approval";
        }
        return match turn.phase() {
            TurnPhase::Compacting => "compacting",
            TurnPhase::Cancelling => "cancelling",
            _ => "running",
        };
    }
    match state.last_outcome() {
        Some(TurnOutcome::Completed) => "completed",
        Some(TurnOutcome::Cancelled) => "cancelled",
        Some(TurnOutcome::Failed { .. }) => "failed",
        None => "idle",
    }
}

fn recursive_preorder(
    root: ThreadId,
    threads: Vec<atra_protocol::Thread>,
) -> Result<Vec<(atra_protocol::Thread, usize)>> {
    let expected = threads.len();
    let mut by_id = threads
        .into_iter()
        .map(|thread| (thread.id, thread))
        .collect::<HashMap<_, _>>();
    let mut children = HashMap::<ThreadId, Vec<ThreadId>>::new();
    for thread in by_id.values() {
        let parent = thread.parent_thread_id.context("broken thread hierarchy")?;
        children.entry(parent).or_default().push(thread.id);
    }
    for siblings in children.values_mut() {
        siblings.sort_by_key(|id| id.0);
    }
    let mut stack = children
        .remove(&root)
        .unwrap_or_default()
        .into_iter()
        .rev()
        .map(|id| (id, 1))
        .collect::<Vec<_>>();
    let mut ordered = Vec::with_capacity(expected);
    while let Some((id, depth)) = stack.pop() {
        let thread = by_id
            .remove(&id)
            .context("thread hierarchy contains a duplicate or cycle")?;
        if let Some(descendants) = children.remove(&id) {
            stack.extend(
                descendants
                    .into_iter()
                    .rev()
                    .map(|child| (child, depth + 1)),
            );
        }
        ordered.push((thread, depth));
    }
    if ordered.len() != expected {
        bail!("thread hierarchy contains an unreachable descendant");
    }
    Ok(ordered)
}

fn normalize_requested_ids(thread_ids: Vec<ThreadId>) -> Vec<ThreadId> {
    let mut seen = HashSet::new();
    thread_ids
        .into_iter()
        .filter(|thread_id| seen.insert(*thread_id))
        .collect()
}

fn normalized_delete_roots(snapshots: &HashMap<ThreadId, Vec<ThreadId>>) -> Vec<ThreadId> {
    let mut roots = snapshots
        .keys()
        .copied()
        .filter(|candidate| {
            !snapshots
                .iter()
                .any(|(other, subtree)| other != candidate && subtree.contains(candidate))
        })
        .collect::<Vec<_>>();
    roots.sort_by_key(|id| id.0);
    roots
}

fn render_report_events(
    selected: &[crate::storage::Event],
    all: &[crate::storage::Event],
) -> Vec<String> {
    let mut pending = Vec::<(EventSequence, &atra_protocol::ToolCallEvent)>::new();
    let mut completed = HashMap::new();
    let mut matched_results = HashMap::new();
    for event in all {
        match &event.data {
            ThreadEventData::ToolCall(call) => pending.push((event.sequence, call)),
            ThreadEventData::ToolResult(result) => {
                if let Some(index) = pending
                    .iter()
                    .position(|(_, call)| tool_result_matches_call(result, call))
                {
                    let (sequence, call) = pending.remove(index);
                    completed.insert(sequence, result);
                    matched_results.insert(event.sequence, call);
                }
            }
            _ => {}
        }
    }
    selected
        .iter()
        .filter_map(|event| match &event.data {
            ThreadEventData::UserMessage(message) => Some(format!("[user]\n{}", message.content)),
            ThreadEventData::AssistantMessage(message) => Some(format!(
                "[assistant/{}]\n{}",
                match message.phase {
                    AssistantMessagePhase::Commentary => "commentary",
                    AssistantMessagePhase::FinalAnswer => "final",
                },
                message.content
            )),
            ThreadEventData::ToolCall(call) => {
                (!completed.contains_key(&event.sequence)).then(|| tool_summary(call, None))
            }
            ThreadEventData::ToolResult(result) => matched_results
                .get(&event.sequence)
                .map(|call| tool_summary(call, Some(result))),
            _ => None,
        })
        .collect()
}

fn tool_result_matches_call(
    result: &atra_protocol::ToolResultEvent,
    call: &atra_protocol::ToolCallEvent,
) -> bool {
    match (result, call) {
        (
            atra_protocol::ToolResultEvent::Custom {
                call_id: result_id, ..
            },
            atra_protocol::ToolCallEvent::Custom { call_id, .. },
        ) => result_id == call_id,
        (
            atra_protocol::ToolResultEvent::Function {
                call_id: result_id, ..
            },
            atra_protocol::ToolCallEvent::Function { call_id, .. },
        ) => result_id == call_id,
        _ => false,
    }
}

fn tool_name(call: &atra_protocol::ToolCallEvent) -> &str {
    match call {
        atra_protocol::ToolCallEvent::Custom { name, .. }
        | atra_protocol::ToolCallEvent::Function { name, .. } => name,
    }
}
fn failed_command_artifact(
    result: &atra_protocol::ToolResultEvent,
) -> Option<&atra_protocol::CommandExecutionArtifact> {
    let artifacts = match result {
        atra_protocol::ToolResultEvent::Custom { artifacts, .. }
        | atra_protocol::ToolResultEvent::Function { artifacts, .. } => artifacts,
    };
    artifacts.iter().find_map(failed_command_in_artifact)
}

fn failed_command_in_artifact(
    artifact: &atra_protocol::ToolArtifact,
) -> Option<&atra_protocol::CommandExecutionArtifact> {
    match artifact {
        atra_protocol::ToolArtifact::CommandExecution(
            command @ atra_protocol::CommandExecutionArtifact::Finished { exit_code, .. },
        ) if *exit_code != Some(0) => Some(command),
        atra_protocol::ToolArtifact::RunnerOperation(operation) => operation
            .artifacts
            .iter()
            .find_map(failed_command_in_artifact),
        atra_protocol::ToolArtifact::CommandExecution(_)
        | atra_protocol::ToolArtifact::PatchOperations(_) => None,
    }
}

fn tool_summary(
    call: &atra_protocol::ToolCallEvent,
    result: Option<&atra_protocol::ToolResultEvent>,
) -> String {
    let arguments = match call {
        atra_protocol::ToolCallEvent::Custom { input, .. } => input.clone(),
        atra_protocol::ToolCallEvent::Function { arguments, .. } => {
            serde_json::to_string(arguments).unwrap_or_default()
        }
    };
    let arguments = one_line(&arguments);
    let name = truncate_chars(&one_line(tool_name(call)), 80);
    let header = format!("[tool {name}]");
    let status = match result.and_then(failed_command_artifact) {
        Some(atra_protocol::CommandExecutionArtifact::Finished {
            output, exit_code, ..
        }) => format!(
            "status=failed exit_code={} error_tail={}",
            exit_code.map_or("unknown".to_owned(), |code| code.to_string()),
            tail_chars(&one_line(output), 240)
        ),
        Some(_) => unreachable!("only finished command failures are returned"),
        None if result.is_some() => "status=ok".to_owned(),
        None => "status=pending".to_owned(),
    };
    let fixed = header.chars().count() + "\narguments=\n".chars().count() + status.chars().count();
    let arguments = truncate_chars(&arguments, 512_usize.saturating_sub(fixed));
    format!("{header}\narguments={arguments}\n{status}")
}

fn one_line(value: &str) -> String {
    let mut output = String::new();
    let mut separated = false;
    for character in value.chars() {
        if character.is_whitespace() || character.is_control() {
            separated = !output.is_empty();
        } else {
            if separated {
                output.push(' ');
                separated = false;
            }
            output.push(character);
        }
    }
    output
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    let length = value.chars().count();
    if length <= maximum {
        return value.to_owned();
    }
    if maximum <= 3 {
        return ".".repeat(maximum);
    }
    let mut output = value.chars().take(maximum - 3).collect::<String>();
    output.push_str("...");
    output
}

fn tail_chars(value: &str, maximum: usize) -> String {
    let length = value.chars().count();
    if length <= maximum {
        return value.to_owned();
    }
    if maximum <= 3 {
        return ".".repeat(maximum);
    }
    let mut output = "...".to_owned();
    output.extend(value.chars().skip(length - (maximum - 3)));
    output
}

fn thread_label(thread: &atra_protocol::Thread) -> String {
    let label = thread
        .display_name
        .as_deref()
        .map(one_line)
        .unwrap_or_default();
    if label.is_empty() {
        "unnamed".to_owned()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, path::PathBuf, sync::Arc, time::Duration};

    use atra_protocol::{
        AgentRequest, ApprovalPolicy, CommandExecutionArtifact, ControllerLifecycle,
        ControllerState, EventSequence, ProcessHandle, ProcessId, ThreadOperation, ToolArtifact,
        ToolCallEvent, ToolResultEvent, TurnPhase,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::{Notify, oneshot};

    use super::*;

    fn thread(id: i64, parent: i64) -> atra_protocol::Thread {
        atra_protocol::Thread {
            id: ThreadId(id),
            parent_thread_id: Some(ThreadId(parent)),
            display_name: Some(format!("thread-{id}")),
            provider: "fake".to_owned(),
            model: "model".to_owned(),
            reasoning_effort: "medium".to_owned(),
        }
    }

    async fn test_state() -> (Arc<State>, TempDir, ThreadId) {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("model.json");
        fs::write(
            &script,
            r#"[{"assistant_message":{"content":"done","phase":"final_answer"}}]"#,
        )
        .unwrap();
        let store = crate::storage::Store::open(&directory.path().join("state.db"))
            .await
            .unwrap();
        let provider = crate::model::fake(&script).unwrap();
        let providers = HashMap::from([(provider.id().to_owned(), provider)]);
        let root = store
            .create_thread(
                Some("root".to_owned()),
                crate::model::FAKE_PROVIDER.to_owned(),
                crate::model::DEFAULT_MODEL.to_owned(),
                "medium".to_owned(),
            )
            .await
            .unwrap();
        let threads = store.threads().await.unwrap();
        let controller = ControllerState::new(
            ControllerLifecycle::Running,
            threads,
            vec![
                crate::provider_state(
                    crate::model::FAKE_PROVIDER,
                    &providers[crate::model::FAKE_PROVIDER],
                )
                .await,
            ],
            Vec::new(),
        );
        let views = Arc::new(crate::views::Views::new(controller));
        let (callback_events, _) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(State {
            runners: Arc::new(crate::runner_pool::RunnerPool::new(
                None,
                Arc::downgrade(&views),
                callback_events,
            )),
            store,
            providers,
            default_provider: crate::model::FAKE_PROVIDER.to_owned(),
            turns: crate::lifecycle::TurnLifecycle::new(),
            execution_contexts: Arc::new(std::sync::Mutex::new(HashMap::from([(
                "context".to_owned(),
                root,
            )]))),
            skill_store: atra_store::Store::open(directory.path().join("objects")).unwrap(),
            skill_generation: tokio::sync::Mutex::new(None),
            data_home: directory.path().to_owned(),
            prompt_cache_namespace: "test".to_owned(),
            workspace: directory.path().to_owned(),
            mutation: Arc::new(tokio::sync::Mutex::new(())),
            views,
        });
        (state, directory, root)
    }

    async fn create_agent(state: &Arc<State>, name: &str) -> ThreadId {
        let response = state
            .handle_agent_request(
                "context",
                AgentRequest::Create {
                    name: name.to_owned(),
                    model: None,
                    effort: None,
                    allow_delegation: false,
                },
            )
            .await;
        assert!(response.success, "{}", response.output);
        ThreadId(
            response
                .output
                .strip_prefix("thread_id=")
                .unwrap()
                .parse()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn recursive_agent_creation_requires_explicit_permission() {
        let (state, _directory, _root) = test_state().await;
        let denied_parent = create_agent(&state, "denied-parent").await;
        let denied = state
            .agent_request(
                denied_parent,
                AgentRequest::Create {
                    name: "denied-child".to_owned(),
                    model: None,
                    effort: None,
                    allow_delegation: false,
                },
            )
            .await
            .unwrap_err();
        assert!(
            denied
                .to_string()
                .contains("not allowed to create child agents")
        );

        let allowed_parent = state
            .handle_agent_request(
                "context",
                AgentRequest::Create {
                    name: "allowed-parent".to_owned(),
                    model: None,
                    effort: None,
                    allow_delegation: true,
                },
            )
            .await;
        assert!(allowed_parent.success, "{}", allowed_parent.output);
        let allowed_parent = ThreadId(
            allowed_parent
                .output
                .strip_prefix("thread_id=")
                .unwrap()
                .parse()
                .unwrap(),
        );
        let child = state
            .agent_request(
                allowed_parent,
                AgentRequest::Create {
                    name: "allowed-child".to_owned(),
                    model: None,
                    effort: None,
                    allow_delegation: false,
                },
            )
            .await
            .unwrap();
        assert!(child.0.starts_with("thread_id="));
    }

    #[test]
    fn agent_list_hierarchy_is_recursive_preorder_with_stable_siblings() {
        let ordered = recursive_preorder(
            ThreadId(1),
            vec![
                thread(6, 2),
                thread(4, 2),
                thread(3, 1),
                thread(5, 3),
                thread(2, 1),
                thread(7, 4),
            ],
        )
        .unwrap();
        assert_eq!(
            ordered
                .into_iter()
                .map(|(thread, depth)| (thread.id, depth))
                .collect::<Vec<_>>(),
            vec![
                (ThreadId(2), 1),
                (ThreadId(4), 2),
                (ThreadId(7), 3),
                (ThreadId(6), 2),
                (ThreadId(3), 1),
                (ThreadId(5), 2),
            ]
        );
    }

    #[test]
    fn cancel_normalizes_duplicate_requested_ids_before_validation() {
        assert_eq!(
            normalize_requested_ids(vec![
                ThreadId(90),
                ThreadId(90),
                ThreadId(4),
                ThreadId(90),
                ThreadId(4),
            ]),
            vec![ThreadId(90), ThreadId(4)]
        );
    }

    #[test]
    fn agent_thread_label_cannot_inject_output_lines() {
        let thread = atra_protocol::Thread {
            id: ThreadId(1),
            parent_thread_id: None,
            display_name: Some("research\n== forged ==\x1b[31m".to_owned()),
            provider: "fake".to_owned(),
            model: "test".to_owned(),
            reasoning_effort: "medium".to_owned(),
        };

        let label = thread_label(&thread);
        assert_eq!(label.lines().count(), 1);
        assert!(!label.contains('\x1b'));
    }

    #[tokio::test]
    async fn accepted_send_finishes_lifecycle_after_callback_abort() {
        let lifecycle = Arc::new(crate::lifecycle::TurnLifecycle::new());
        let thread_id = ThreadId(1);
        let gate = Arc::new(Notify::new());
        let (registered, registration) = oneshot::channel();
        let owned_lifecycle = Arc::clone(&lifecycle);
        let owned_gate = Arc::clone(&gate);
        let callback = tokio::spawn(run_callback_operation("test send", async move {
            owned_lifecycle.start(thread_id).unwrap();
            registered.send(()).unwrap();
            Ok::<_, anyhow::Error>(async move {
                owned_gate.notified().await;
                owned_lifecycle.finish(thread_id);
            })
        }));

        registration.await.unwrap();
        callback.abort();
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while lifecycle.get(thread_id).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned send operation did not converge");
    }

    #[tokio::test]
    async fn accepted_cancel_requests_same_turn_after_callback_abort() {
        let lifecycle = Arc::new(crate::lifecycle::TurnLifecycle::new());
        let thread_id = ThreadId(1);
        let turn = lifecycle.start(thread_id).unwrap();
        let gate = Arc::new(Notify::new());
        let (begun, beginning) = oneshot::channel();
        let owned_lifecycle = Arc::clone(&lifecycle);
        let owned_gate = Arc::clone(&gate);
        let callback = tokio::spawn(run_callback_operation("test cancel", async move {
            let captured = owned_lifecycle.begin_cancellation_many(&[thread_id]);
            assert_eq!(captured, vec![thread_id]);
            begun.send(()).unwrap();
            Ok::<_, anyhow::Error>(async move {
                owned_gate.notified().await;
            })
        }));

        beginning.await.unwrap();
        callback.abort();
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(1), turn.cancelled())
            .await
            .expect("accepted cancellation was not latched");
    }

    #[tokio::test]
    async fn accepted_cancel_is_latched_before_concurrent_shutdown_after_callback_abort() {
        let (state, _directory, _root) = test_state().await;
        let child = create_agent(&state, "child").await;
        state.materialize_thread(child).await.unwrap();
        let turn = state.turns.start(child).unwrap();
        state
            .views
            .apply_thread(
                child,
                ThreadOperation::ActiveTurnStarted {
                    phase: TurnPhase::Running,
                },
            )
            .await
            .unwrap();
        let callback_state = Arc::clone(&state);
        let callback = tokio::spawn(run_callback_operation("test cancel", async move {
            callback_state
                .accept_agent_cancellation(ThreadId(1), vec![child], false)
                .await
        }));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !turn.is_cancelling() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        callback.abort();

        tokio::time::timeout(Duration::from_secs(1), state.shutdown())
            .await
            .expect("shutdown was starved by accepted cancellation")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), turn.cancelled())
            .await
            .expect("accepted cancellation was overtaken by shutdown");
    }

    #[tokio::test]
    async fn delete_with_unresponsive_runner_does_not_starve_other_mutations_or_shutdown() {
        let (state, directory, _root) = test_state().await;
        let deleted = create_agent(&state, "deleted").await;

        let runner_script = directory.path().join("unresponsive-runner.sh");
        let stop_runner = directory.path().join("stop-runner");
        fs::write(
            &runner_script,
            format!(
                "read -r line\nprintf '%s\\n' '{{\"message\":\"response\",\"payload\":{{\"request_id\":0,\"status\":\"ready\"}}}}'\n(while [ ! -e '{}' ]; do sleep 0.01; done; kill -TERM $$) &\nwhile read -r line; do :; done\n",
                stop_runner.display()
            ),
        )
        .unwrap();
        state
            .runners
            .launch(
                "stuck".to_owned(),
                "test".to_owned(),
                ApprovalPolicy::Allow,
                vec!["bash".to_owned(), runner_script.display().to_string()],
                &state.skill_store,
                &crate::skills::SkillGeneration {
                    manifest: atra_store::TreeManifest {
                        entries: Vec::new(),
                    },
                    prompt: None,
                    skills: Vec::new(),
                },
            )
            .await
            .unwrap();
        state
            .runners
            .insert_process(
                crate::runner_pool::ProcessKey {
                    thread_id: deleted,
                    runner: "stuck".to_owned(),
                    process_id: ProcessId("process".to_owned()),
                },
                crate::runner_pool::ProcessRecord {
                    handle: ProcessHandle("handle".to_owned()),
                    command: "sleep".to_owned(),
                    started_at_ms: 0,
                },
            )
            .await;
        state
            .execution_contexts
            .lock()
            .unwrap()
            .insert("deleted-context".to_owned(), deleted);
        let old_key = crate::runner_pool::ProcessKey {
            thread_id: deleted,
            runner: "stuck".to_owned(),
            process_id: ProcessId("process".to_owned()),
        };

        let delete_state = Arc::clone(&state);
        let deletion = tokio::spawn(async move {
            delete_state
                .handle_agent_request(
                    "context",
                    AgentRequest::Delete {
                        thread_ids: vec![deleted],
                        recursive: false,
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.store.thread(deleted).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delete did not commit before Runner cleanup");
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.runners.process(&old_key).await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("accepted deletion did not detach the old process");
        assert!(
            !deletion.is_finished(),
            "fake Runner unexpectedly answered stop"
        );
        assert!(
            state.runners.process(&old_key).await.is_none(),
            "accepted deletion left the old process publicly reachable"
        );

        let create_under_deleted = state
            .handle_agent_request(
                "deleted-context",
                AgentRequest::Create {
                    name: "must-not-resurrect".to_owned(),
                    model: None,
                    effort: None,
                    allow_delegation: false,
                },
            )
            .await;
        assert!(!create_under_deleted.success);

        let surviving =
            tokio::time::timeout(Duration::from_secs(1), create_agent(&state, "survivor"))
                .await
                .expect("unrelated mutation was starved by Runner cleanup");
        assert!(surviving.0 > deleted.0, "deleted ThreadId was reused");
        assert!(state.store.thread(surviving).await.is_ok());
        let surviving_key = crate::runner_pool::ProcessKey {
            thread_id: surviving,
            runner: "stuck".to_owned(),
            process_id: ProcessId("new-process".to_owned()),
        };
        assert!(
            state
                .runners
                .insert_process(
                    surviving_key.clone(),
                    crate::runner_pool::ProcessRecord {
                        handle: ProcessHandle("new-handle".to_owned()),
                        command: "new sleep".to_owned(),
                        started_at_ms: 1,
                    },
                )
                .await
        );
        tokio::time::timeout(Duration::from_secs(1), state.shutdown())
            .await
            .expect("shutdown was starved by Runner cleanup")
            .unwrap();

        fs::write(stop_runner, "").unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), deletion)
            .await
            .expect("delete did not finish after Runner disconnected")
            .unwrap();
        assert!(response.success, "{}", response.output);
        assert!(
            state.runners.process(&surviving_key).await.is_some(),
            "old cleanup consumed a process belonging to the new thread"
        );
    }

    #[tokio::test]
    async fn handle_agent_request_enforces_context_and_persists_create_delete() {
        let (state, _directory, root) = test_state().await;
        let child = create_agent(&state, "child").await;
        assert_eq!(
            state.store.thread(child).await.unwrap().parent_thread_id,
            Some(root)
        );

        let unauthorized = state
            .handle_agent_request(
                "missing",
                AgentRequest::Delete {
                    thread_ids: vec![child],
                    recursive: false,
                },
            )
            .await;
        assert!(!unauthorized.success);
        assert!(state.store.thread(child).await.is_ok());

        let deleted = state
            .handle_agent_request(
                "context",
                AgentRequest::Delete {
                    thread_ids: vec![child],
                    recursive: false,
                },
            )
            .await;
        assert!(deleted.success, "{}", deleted.output);
        assert!(state.store.thread(child).await.is_err());
        assert!(!state.views.has_thread(child).await);
    }

    #[tokio::test]
    async fn handle_agent_request_send_registers_and_finishes_a_real_child_turn() {
        let (state, _directory, _root) = test_state().await;
        let child = create_agent(&state, "child").await;

        let sent = state
            .handle_agent_request(
                "context",
                AgentRequest::Send {
                    thread_id: child,
                    message: "work".to_owned(),
                },
            )
            .await;
        assert!(sent.success, "{}", sent.output);
        assert_eq!(sent.output, "after_sequence=0");
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.turns.get(child).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("accepted child turn did not finish");
        let events = state.store.events(child).await.unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.data,
            ThreadEventData::AssistantMessage(message) if message.content == "done"
        )));
    }

    #[tokio::test]
    async fn cancellation_before_acceptance_does_not_start_the_operation() {
        let lifecycle = Arc::new(crate::lifecycle::TurnLifecycle::new());
        let gate = Arc::new(Notify::new());
        let (waiting, wait_started) = oneshot::channel();
        let accepted_lifecycle = Arc::clone(&lifecycle);
        let accepted_gate = Arc::clone(&gate);
        let callback = tokio::spawn(run_callback_operation("test preaccept", async move {
            waiting.send(()).unwrap();
            accepted_gate.notified().await;
            accepted_lifecycle.start(ThreadId(1))?;
            Ok::<_, anyhow::Error>(async move {
                accepted_lifecycle.finish(ThreadId(1));
            })
        }));

        wait_started.await.unwrap();
        callback.abort();
        gate.notify_one();
        tokio::task::yield_now().await;
        assert!(lifecycle.get(ThreadId(1)).is_none());
    }

    #[test]
    fn tool_report_pairs_by_call_id_and_uses_command_exit_status() {
        let call = ToolCallEvent::Function {
            name: "command".to_owned(),
            arguments: json!({"cmd": "false"}),
            call_id: "call-1".to_owned(),
        };
        let result = ToolResultEvent::Function {
            name: "command".to_owned(),
            call_id: "call-1".to_owned(),
            result: json!({"success": true, "large": "body must not be shown"}),
            artifacts: vec![ToolArtifact::CommandExecution(
                CommandExecutionArtifact::Finished {
                    output: "first line\nlast error".to_owned(),
                    exit_code: Some(7),
                    runner: "sandbox".to_owned(),
                    full_output_path: PathBuf::from("output"),
                },
            )],
            masked_result: None,
        };
        let events = vec![
            crate::storage::Event {
                sequence: EventSequence(0),
                data: ThreadEventData::ToolCall(call),
            },
            crate::storage::Event {
                sequence: EventSequence(1),
                data: ThreadEventData::ToolResult(result),
            },
        ];

        let rendered = render_report_events(&events, &events);
        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("status=failed exit_code=7"));
        assert!(rendered[0].contains("last error"));
        assert!(!rendered[0].contains("body must not be shown"));
        assert!(rendered[0].lines().count() <= 3);
        assert!(rendered[0].len() <= 512);
    }

    #[test]
    fn successful_tool_report_omits_result_body() {
        let call = ToolCallEvent::Function {
            name: "lookup".to_owned(),
            arguments: json!({"target": "record"}),
            call_id: "call-1".to_owned(),
        };
        let result = ToolResultEvent::Function {
            name: "lookup".to_owned(),
            call_id: "call-1".to_owned(),
            result: json!({"secret_result_body": "omitted"}),
            artifacts: vec![],
            masked_result: None,
        };
        let events = vec![
            crate::storage::Event {
                sequence: EventSequence(0),
                data: ThreadEventData::ToolCall(call),
            },
            crate::storage::Event {
                sequence: EventSequence(1),
                data: ThreadEventData::ToolResult(result),
            },
        ];

        assert_eq!(
            render_report_events(&events, &events),
            vec!["[tool lookup]\narguments={\"target\":\"record\"}\nstatus=ok"]
        );
    }

    #[test]
    fn successful_command_output_containing_error_is_not_a_failure() {
        let call = ToolCallEvent::Function {
            name: "command".to_owned(),
            arguments: json!({"cmd": "printf error"}),
            call_id: "call-1".to_owned(),
        };
        let result = ToolResultEvent::Function {
            name: "command".to_owned(),
            call_id: "call-1".to_owned(),
            result: json!("error is expected output"),
            artifacts: vec![ToolArtifact::CommandExecution(
                CommandExecutionArtifact::Finished {
                    output: "error is expected output".to_owned(),
                    exit_code: Some(0),
                    runner: "sandbox".to_owned(),
                    full_output_path: PathBuf::from("output"),
                },
            )],
            masked_result: None,
        };
        let events = vec![
            crate::storage::Event {
                sequence: EventSequence(0),
                data: ThreadEventData::ToolCall(call),
            },
            crate::storage::Event {
                sequence: EventSequence(1),
                data: ThreadEventData::ToolResult(result),
            },
        ];

        assert_eq!(
            render_report_events(&events, &events),
            vec!["[tool command]\narguments={\"cmd\":\"printf error\"}\nstatus=ok"]
        );
    }

    #[test]
    fn nested_command_failure_uses_artifact_status() {
        let call = ToolCallEvent::Custom {
            item_id: Some("item-1".to_owned()),
            name: "command".to_owned(),
            input: "runner=sandbox\nfalse".to_owned(),
            call_id: "call-1".to_owned(),
        };
        let command = CommandExecutionArtifact::Finished {
            output: "plain failure output".to_owned(),
            exit_code: Some(7),
            runner: "sandbox".to_owned(),
            full_output_path: PathBuf::from("output"),
        };
        let result = ToolResultEvent::Custom {
            name: "command".to_owned(),
            call_id: "call-1".to_owned(),
            result: json!("Process exited with code 7"),
            artifacts: vec![ToolArtifact::RunnerOperation(
                atra_protocol::RunnerOperationArtifact {
                    operation: 1,
                    runner: "sandbox".to_owned(),
                    label: "false".to_owned(),
                    result: json!("Process exited with code 7"),
                    artifacts: vec![ToolArtifact::CommandExecution(command)],
                },
            )],
            masked_result: None,
        };
        let events = vec![
            crate::storage::Event {
                sequence: EventSequence(0),
                data: ThreadEventData::ToolCall(call),
            },
            crate::storage::Event {
                sequence: EventSequence(1),
                data: ThreadEventData::ToolResult(result),
            },
        ];
        let rendered = render_report_events(&events, &events);

        assert!(rendered[0].contains("status=failed exit_code=7"));
        assert!(rendered[0].contains("plain failure output"));
    }

    #[test]
    fn long_multibyte_arguments_keep_status_within_character_limit() {
        let call = ToolCallEvent::Function {
            name: "lookup".to_owned(),
            arguments: json!({"query": "検索".repeat(400)}),
            call_id: "call-1".to_owned(),
        };
        let result = ToolResultEvent::Function {
            name: "lookup".to_owned(),
            call_id: "call-1".to_owned(),
            result: json!({"body": "omitted"}),
            artifacts: vec![],
            masked_result: None,
        };
        let events = vec![
            crate::storage::Event {
                sequence: EventSequence(0),
                data: ThreadEventData::ToolCall(call),
            },
            crate::storage::Event {
                sequence: EventSequence(1),
                data: ThreadEventData::ToolResult(result),
            },
        ];
        let rendered = render_report_events(&events, &events);

        assert_eq!(rendered[0].lines().count(), 3);
        assert!(rendered[0].ends_with("status=ok"));
        assert!(rendered[0].chars().count() <= 512);
    }

    #[test]
    fn overlapping_delete_snapshots_have_argument_independent_roots() {
        let parent = ThreadId(2);
        let child = ThreadId(3);
        let sibling = ThreadId(4);
        let first = HashMap::from([
            (child, vec![child]),
            (parent, vec![parent, child]),
            (sibling, vec![sibling]),
        ]);
        let second = HashMap::from([
            (sibling, vec![sibling]),
            (parent, vec![child, parent]),
            (child, vec![child]),
        ]);

        assert_eq!(normalized_delete_roots(&first), vec![parent, sibling]);
        assert_eq!(
            normalized_delete_roots(&first),
            normalized_delete_roots(&second)
        );
    }
}
