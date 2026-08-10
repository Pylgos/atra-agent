use anyhow::{Result, bail};
use atra_client::{CancelResult, ProviderLoginStatus, TurnEvent, TurnResult};
use atra_protocol::ThreadEventData;
use tokio::sync::mpsc;

use super::{Activity, App, HistoryChange, Target, ThreadView, TurnUpdate};
use crate::{
    notification,
    runtime::Effect,
    state::{
        Approval, ApprovalState, CheckpointPicker, FocusPane, Overlay, ThreadPickerState, TurnState,
    },
    transcript::{Author, TranscriptEntry, sanitize},
};

impl App {
    pub(crate) fn update(
        &mut self,
        update: TurnUpdate,
        effects: &mpsc::UnboundedSender<Effect>,
    ) -> Result<()> {
        match update {
            TurnUpdate::Started {
                message,
                thread_id,
                threads,
            } => {
                self.threads = threads;
                if self.target.thread_id().is_none() {
                    self.target = Target::Thread {
                        id: thread_id,
                        view: ThreadView::Live,
                    };
                }
                if let Some(thread) = self
                    .threads
                    .iter_mut()
                    .find(|thread| thread.id == thread_id)
                {
                    thread.display_name = Some(message);
                }
                return Ok(());
            }
            TurnUpdate::Stream(update) => {
                self.apply_turn_update(update, effects)?;
                return Ok(());
            }
            TurnUpdate::StreamFailed(error) => {
                self.transcript.discard_live_previews();
                self.turn = TurnState::Idle;
                self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                return Ok(());
            }
            TurnUpdate::Compacted { thread_id, result } => {
                self.turn = TurnState::Idle;
                if self.target.thread_id() != Some(thread_id) {
                    return Ok(());
                }
                match result {
                    Ok((transcript, events)) => {
                        self.transcript.replace(transcript, events);
                        self.clear_selection();
                        self.reset_view();
                        self.metrics_stale = false;
                        self.activity = Some(Activity::Info("Thread compacted".to_owned()));
                    }
                    Err(error) => {
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                return Ok(());
            }
            TurnUpdate::ApprovalResolved {
                approval_id,
                result,
            } => {
                match result {
                    Ok(TurnResult::ApprovalResolved) => {}
                    Ok(result) => {
                        bail!("controller returned an unexpected approval result: {result:?}")
                    }
                    Err(error) => {
                        self.restore_failed_approval(approval_id);
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                return Ok(());
            }
            TurnUpdate::CancelCompleted { thread_id, result } => {
                if self.target.thread_id() == Some(thread_id) {
                    match result {
                        Ok(CancelResult::Cancelled) => {}
                        Ok(CancelResult::NotActive) => {
                            self.turn = TurnState::Idle;
                            self.activity =
                                Some(Activity::Info("Turn already finished".to_owned()));
                        }
                        Err(error) => {
                            self.turn = TurnState::Idle;
                            self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                        }
                    }
                }
                return Ok(());
            }
            TurnUpdate::LoginCompleted(result) => {
                match result {
                    Ok(ProviderLoginStatus::LoggedIn { .. }) => {
                        self.login_required = false;
                        if !self.rate_limit_refresh_pending
                            && self.selected_provider() == Some("codex")
                        {
                            self.rate_limit_refresh_pending = true;
                            effects
                                .send(Effect::PollRateLimits {
                                    endpoint: self.endpoint.clone(),
                                    provider: "codex".to_owned(),
                                })
                                .ok();
                        }
                        self.activity = Some(Activity::Info("Codex login complete".to_owned()));
                    }
                    Ok(ProviderLoginStatus::LoginRequired) => {
                        self.login_required = true;
                        self.activity = Some(Activity::Info("Codex login required".to_owned()));
                    }
                    Err(error) => {
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                return Ok(());
            }
            TurnUpdate::RateLimitsLoaded { provider, result } => {
                self.rate_limit_refresh_pending = false;
                if self.selected_provider() == Some(provider.as_str())
                    && let Ok(snapshots) = result
                {
                    self.rate_limits = snapshots;
                }
                return Ok(());
            }
            TurnUpdate::ThreadSelected { thread_id, result } => {
                let (transcript, events) = result?;
                self.target = Target::Thread {
                    id: thread_id,
                    view: ThreadView::Live,
                };
                self.processes.clear();
                self.transcript.replace(transcript, events);
                self.overlay = Overlay::None;
                self.clear_selection();
                self.reset_view();
                self.metrics_stale = false;
                self.rate_limits = serde_json::Value::Array(Vec::new());
                self.activity = Some(Activity::Info("Thread selected".to_owned()));
                return Ok(());
            }
            TurnUpdate::ProcessesLoaded { thread_id, result } => {
                self.process_refresh_pending = false;
                if self.target.thread_id() != Some(thread_id) {
                    return Ok(());
                }
                let (processes, detail) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        if matches!(self.overlay, Overlay::Processes(_)) {
                            self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                        }
                        return Ok(());
                    }
                };
                let selected_key = match &self.overlay {
                    Overlay::Processes(picker) => self
                        .processes
                        .get(picker.selected)
                        .map(|process| (process.runner.clone(), process.process_id.clone())),
                    _ => None,
                };
                self.processes = processes;
                if let Overlay::Processes(picker) = &mut self.overlay {
                    picker.selected = selected_key
                        .and_then(|(runner, process_id)| {
                            self.processes.iter().position(|process| {
                                process.runner == runner && process.process_id == process_id
                            })
                        })
                        .unwrap_or_else(|| {
                            picker.selected.min(self.processes.len().saturating_sub(1))
                        });
                    let selected = self.processes.get(picker.selected);
                    picker.detail = detail.filter(|detail| {
                        selected.is_some_and(|selected| {
                            detail.process.runner == selected.runner
                                && detail.process.process_id == selected.process_id
                        })
                    });
                }
                return Ok(());
            }
            TurnUpdate::ProcessStopped {
                thread_id,
                runner,
                process_id,
                result,
            } => {
                if self.target.thread_id() != Some(thread_id) {
                    return Ok(());
                }
                match result {
                    Ok(()) => {
                        self.processes.retain(|process| {
                            process.runner != runner || process.process_id != process_id
                        });
                        if let Overlay::Processes(picker) = &mut self.overlay {
                            picker.selected =
                                picker.selected.min(self.processes.len().saturating_sub(1));
                            picker.detail = None;
                            picker.output_scroll = 0;
                        }
                        self.activity = Some(Activity::Info(format!("Stopped {process_id}")));
                    }
                    Err(error) => {
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                return Ok(());
            }
            TurnUpdate::ThreadRenamed {
                thread_id,
                display_name,
                result,
            } => {
                match result {
                    Ok(()) => {
                        if let Some(thread) = self
                            .threads
                            .iter_mut()
                            .find(|thread| thread.id == thread_id)
                        {
                            thread.display_name = Some(display_name);
                        }
                        self.activity = Some(Activity::Info("Thread renamed".to_owned()));
                    }
                    Err(error) => {
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                return Ok(());
            }
            TurnUpdate::ThreadDeleted { thread_id, result } => {
                match result {
                    Ok(()) => {
                        self.threads.retain(|thread| thread.id != thread_id);
                        if self.target.thread_id() == Some(thread_id) {
                            self.reset_to_new_thread();
                        }
                        if self.threads.is_empty()
                            && matches!(self.overlay, Overlay::ThreadPicker(_))
                        {
                            self.overlay = Overlay::None;
                        } else if let Overlay::ThreadPicker(picker) = &mut self.overlay {
                            picker.selected =
                                picker.selected.min(self.threads.len().saturating_sub(1));
                            picker.state = ThreadPickerState::Browsing;
                        }
                        self.activity = Some(Activity::Info("Thread deleted".to_owned()));
                    }
                    Err(error) => {
                        if let Overlay::ThreadPicker(picker) = &mut self.overlay {
                            picker.state = ThreadPickerState::Browsing;
                        }
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                return Ok(());
            }
            TurnUpdate::ModelChanged {
                thread_id,
                provider,
                model,
                reasoning_effort,
                result,
            } => {
                match result {
                    Ok(()) => {
                        if let Some(thread) = self
                            .threads
                            .iter_mut()
                            .find(|thread| thread.id == thread_id)
                        {
                            thread.provider = provider;
                            thread.model = model;
                            thread.reasoning_effort = reasoning_effort;
                        }
                        self.metrics_stale = true;
                        self.rate_limits = serde_json::Value::Array(Vec::new());
                        self.activity = Some(Activity::Info("Thread model changed".to_owned()));
                    }
                    Err(error) => {
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                return Ok(());
            }
            TurnUpdate::CheckpointsLoaded { thread_id, result } => {
                if self.target.thread_id() != Some(thread_id) {
                    return Ok(());
                }
                let (checkpoints, events) = result?;
                if checkpoints.is_empty() {
                    self.activity = Some(Activity::Info("No checkpoints are available".to_owned()));
                } else {
                    let checkpoint = checkpoints[0].clone();
                    self.transcript.replace_events(events);
                    self.target = Target::Thread {
                        id: thread_id,
                        view: ThreadView::Checkpoint {
                            checkpoint,
                            picker: CheckpointPicker {
                                checkpoints,
                                selected: 0,
                            },
                        },
                    };
                    self.clear_selection();
                    self.reset_view();
                    self.view.focus = FocusPane::Checkpoints;
                    self.activity = Some(Activity::Info(
                        "Browse checkpoints · Tab switches pane · Esc returns".to_owned(),
                    ));
                }
                return Ok(());
            }
            TurnUpdate::CheckpointLoaded(result) => {
                let (checkpoint, events) = result?;
                if self.target.thread_id() != Some(checkpoint.thread_id) {
                    return Ok(());
                }
                if let Some(picker) = self.target.checkpoint_picker()
                    && picker.checkpoints[picker.selected].id != checkpoint.id
                {
                    return Ok(());
                }
                self.transcript.replace_events(events);
                let Target::Thread {
                    view:
                        ThreadView::Checkpoint {
                            checkpoint: displayed,
                            ..
                        },
                    ..
                } = &mut self.target
                else {
                    return Ok(());
                };
                *displayed = checkpoint;
                self.clear_selection();
                self.reset_view();
                if self.target.checkpoint_picker().is_some() {
                    self.view.focus = FocusPane::Checkpoints;
                }
                self.activity = Some(Activity::Info(
                    "Browse checkpoints · Tab switches pane · Esc returns".to_owned(),
                ));
                return Ok(());
            }
            TurnUpdate::HistoryChanged {
                source_thread_id,
                draft,
                result,
            } => {
                if self.target.thread_id() != Some(source_thread_id) {
                    return Ok(());
                }
                let HistoryChange {
                    message,
                    thread_id,
                    threads,
                    transcript,
                    events,
                } = result?;
                self.target = Target::Thread {
                    id: thread_id,
                    view: ThreadView::Live,
                };
                self.processes.clear();
                self.threads = threads;
                self.transcript.replace(transcript, events);
                self.overlay = Overlay::None;
                self.clear_selection();
                self.reset_view();
                if let Some(draft) = draft {
                    self.message_input.set(draft);
                    self.view.focus = FocusPane::Input;
                }
                self.metrics_stale = false;
                self.activity = Some(Activity::Info(message));
                return Ok(());
            }
        }
    }

    fn apply_turn_update(
        &mut self,
        update: atra_client::TurnUpdate,
        effects: &mpsc::UnboundedSender<Effect>,
    ) -> Result<()> {
        let thread_id = update.thread_id;
        match update.event {
            TurnEvent::Started => {
                if self.target.thread_id() == Some(thread_id) {
                    if matches!(self.turn, TurnState::Cancelling) {
                        effects
                            .send(Effect::CancelTurn {
                                endpoint: self.endpoint.clone(),
                                thread_id,
                            })
                            .ok();
                    } else {
                        self.turn = TurnState::Running;
                        self.activity = Some(Activity::Info(
                            "Waiting for Atra Controller… · Esc cancels".to_owned(),
                        ));
                    }
                }
            }
            TurnEvent::Retry { current, max } => {
                if self.target.thread_id() == Some(thread_id) {
                    self.transcript.discard_live_previews();
                    self.activity = Some(Activity::Info(format!(
                        "Reconnecting... {current}/{max} · Esc cancels"
                    )));
                }
            }
            TurnEvent::Delta { content } => {
                if self.target.thread_id() == Some(thread_id) {
                    self.transcript.append_assistant_delta(&content);
                }
            }
            TurnEvent::ReasoningSummaryDelta { content } => {
                if self.target.thread_id() == Some(thread_id) {
                    self.transcript.append_reasoning_delta(&content);
                }
            }
            TurnEvent::ReasoningSummaryPartAdded => {
                if self.target.thread_id() == Some(thread_id) {
                    self.transcript.finish_reasoning_part();
                }
            }
            TurnEvent::WebSearchUpdate { item_id, action } => {
                if self.target.thread_id() == Some(thread_id) {
                    self.transcript.update_web_search_preview(item_id, action);
                }
            }
            TurnEvent::ToolCallStarted { item_id, name } => {
                if self.target.thread_id() == Some(thread_id) {
                    self.transcript.start_tool_preview(item_id, &name);
                }
            }
            TurnEvent::ToolCallDelta { item_id, delta } => {
                if self.target.thread_id() == Some(thread_id) {
                    self.transcript.append_tool_preview(&item_id, &delta);
                }
            }
            TurnEvent::Event { event } => {
                if self.target.thread_id() == Some(thread_id) {
                    if let ThreadEventData::RateLimits(rate_limits) = &event.data {
                        self.rate_limits = rate_limits.snapshots.clone();
                    }
                    if matches!(&event.data, ThreadEventData::TokenUsage(_)) {
                        self.metrics_stale = false;
                    }
                    self.transcript.apply_event(event);
                }
            }
            TurnEvent::RunnerOperation {
                call_id,
                operation_index,
                update,
            } => {
                if self.target.thread_id() == Some(thread_id) {
                    self.transcript
                        .update_runner_operation(&call_id, operation_index, update);
                }
            }
            TurnEvent::ApprovalRequired {
                approval_id,
                tool,
                arguments,
                operation_index,
                operation_label,
            } => {
                let _ = notification::send("Approval required");
                if self.target.thread_id() == Some(thread_id) {
                    if operation_index.is_some() {
                        self.transcript.set_pending_approval(operation_index);
                    }
                    let runner = arguments
                        .get("runner")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    self.turn = TurnState::AwaitingApproval(Approval {
                        id: approval_id,
                        runner,
                        label: operation_label.unwrap_or(tool),
                        operation_index,
                        state: ApprovalState::Pending,
                    });
                    self.activity = None;
                }
            }
            TurnEvent::Finished(result) => {
                if let TurnResult::Completed { content } = &result {
                    let _ = notification::send(content);
                }
                self.turn = if matches!(result, TurnResult::Compacted) {
                    TurnState::Reloading
                } else {
                    TurnState::Idle
                };
                if self.target.thread_id() == Some(thread_id) {
                    match result {
                        TurnResult::Completed { .. }
                            if self
                                .transcript
                                .entries
                                .last()
                                .is_some_and(TranscriptEntry::is_assistant_message) =>
                        {
                            self.activity = None;
                        }
                        result => self.accept_turn_result(result)?,
                    }
                } else {
                    self.activity = None;
                }
            }
        }
        Ok(())
    }

    fn accept_turn_result(&mut self, result: TurnResult) -> Result<()> {
        match result {
            TurnResult::Completed { content } => {
                self.transcript.entries.push(TranscriptEntry::message(
                    Author::Assistant,
                    sanitize(&content),
                ));
                self.activity = None;
            }
            TurnResult::Cancelled => {
                self.activity = Some(Activity::Info("Cancelled".to_owned()));
            }
            TurnResult::Compacted => {
                self.activity = Some(Activity::Info("Reloading compacted thread…".to_owned()));
            }
            result => bail!("controller returned an unexpected turn result: {result:?}"),
        }
        Ok(())
    }
}
