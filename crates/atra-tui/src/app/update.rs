use anyhow::Result;
use atra_protocol::{ControllerChange, ThreadChange, TurnPhase};

use super::{App, HistoryChange, Target, ThreadView, TurnUpdate};
use crate::state::{CheckpointPicker, FocusPane, Overlay, ThreadPickerState, TurnState};

impl App {
    pub(crate) fn apply_controller_change(&mut self, _change: ControllerChange) {
        let state = self.controller_subscription.state();
        self.login_required = state
            .providers()
            .iter()
            .find(|provider| provider.id() == "codex")
            .is_none_or(|provider| {
                !matches!(
                    provider.lifecycle(),
                    atra_protocol::ProviderLifecycle::LoggedIn { .. }
                )
            });
    }

    pub(crate) fn apply_thread_change(&mut self, change: ThreadChange) {
        let Some(subscription) = &self.thread_subscription else {
            return;
        };
        let state = subscription.state();
        self.transcript.apply_change(state, &change);
        let active = state.active_turn().is_some();
        let cancelling = state
            .active_turn()
            .is_some_and(|turn| turn.phase() == TurnPhase::Cancelling);
        let pending_approval = state
            .active_turn()
            .and_then(|turn| turn.pending_approval())
            .map(|approval| approval.id());
        let pending_question_request = state
            .active_turn()
            .and_then(|turn| turn.pending_question())
            .cloned();
        let pending_question = pending_question_request.as_ref().map(|request| request.id);
        self.turn = match std::mem::take(&mut self.turn) {
            TurnState::Starting { .. } if active && pending_question.is_none() => TurnState::Idle,
            TurnState::Cancelling if cancelling || !active => TurnState::Idle,
            TurnState::EnteringDenyReason {
                approval_id,
                reason,
            } if pending_approval == Some(approval_id) => TurnState::EnteringDenyReason {
                approval_id,
                reason,
            },
            TurnState::ResolvingApproval { approval_id }
                if pending_approval == Some(approval_id) =>
            {
                TurnState::ResolvingApproval { approval_id }
            }
            TurnState::EnteringDenyReason { .. } | TurnState::ResolvingApproval { .. } => {
                TurnState::Idle
            }
            TurnState::AnsweringQuestions(form) if pending_question == Some(form.id()) => {
                TurnState::AnsweringQuestions(form)
            }
            TurnState::AnsweringQuestions(_) if pending_question.is_none() => TurnState::Idle,
            _ if pending_question.is_some() => TurnState::AnsweringQuestions(
                crate::state::QuestionForm::new(pending_question_request.unwrap()),
            ),
            _state if !active => TurnState::Idle,
            state => state,
        };
    }

    pub(crate) fn update(&mut self, update: TurnUpdate) -> Result<()> {
        match update {
            TurnUpdate::Started {
                thread_id,
                subscription,
            } => {
                self.transcript.rebuild(subscription.state());
                self.thread_subscription = Some(subscription.into());
                if self.target.thread_id().is_none() {
                    self.target = Target::Thread {
                        id: thread_id,
                        view: ThreadView::Live,
                    };
                }
                Ok(())
            }
            TurnUpdate::StreamFailed(error) => {
                if let Some(subscription) = &self.thread_subscription {
                    self.transcript.rebuild(subscription.state());
                }
                self.sync_turn_interaction();
                self.error = Some(error);
                Ok(())
            }
            TurnUpdate::ApprovalResolved {
                approval_id,
                result,
            } => {
                match result {
                    Ok(()) => {}
                    Err(error) => {
                        self.restore_failed_approval(approval_id);
                        self.error = Some(error);
                    }
                }
                Ok(())
            }
            TurnUpdate::QuestionResolved { request_id, result } => {
                if let Err(error) = result {
                    if let TurnState::AnsweringQuestions(form) = &mut self.turn
                        && form.id() == request_id
                    {
                        form.mode = crate::state::QuestionFormMode::Confirm;
                    }
                    self.error = Some(error);
                }
                Ok(())
            }
            TurnUpdate::CancelCompleted { thread_id, result } => {
                if self.target.thread_id() == Some(thread_id) {
                    match result {
                        Ok(()) => {}
                        Err(error) => {
                            self.sync_turn_interaction();
                            self.error = Some(error);
                        }
                    }
                }
                Ok(())
            }
            TurnUpdate::LoginCompleted(result) => {
                self.login_pending = false;
                match result {
                    Ok(()) => {
                        self.login_required = false;
                    }
                    Err(error) => {
                        self.error = Some(error);
                    }
                }
                Ok(())
            }
            TurnUpdate::ThreadSelected { thread_id, result } => {
                let subscription = match result {
                    Ok(subscription) => subscription,
                    Err(error) => {
                        if let Overlay::ThreadPicker(picker) = &mut self.overlay {
                            picker.state = ThreadPickerState::Browsing;
                        }
                        self.error = Some(error);
                        return Ok(());
                    }
                };
                self.transcript.rebuild(subscription.state());
                self.thread_subscription = Some(subscription.into());
                self.sync_turn_interaction();
                self.target = Target::Thread {
                    id: thread_id,
                    view: ThreadView::Live,
                };
                self.process_subscription = None;
                self.overlay = Overlay::None;
                self.clear_selection();
                self.reset_view();
                Ok(())
            }
            TurnUpdate::ProcessesLoaded { thread_id, result } => {
                self.process_selection_pending = false;
                if self.target.thread_id() != Some(thread_id) {
                    return Ok(());
                }
                let subscription = match result {
                    Ok(result) => result,
                    Err(error) => {
                        if matches!(self.overlay, Overlay::Processes(_)) {
                            self.error = Some(error);
                        }
                        return Ok(());
                    }
                };
                self.process_subscription = subscription.map(Into::into);
                let process_count = self.processes().len();
                let selected_index = self.process_subscription.as_ref().and_then(|subscription| {
                    let locator = subscription.state().process().locator();
                    self.processes().iter().position(|process| {
                        process.locator().runner() == locator.runner()
                            && process.locator().process_id() == locator.process_id()
                    })
                });
                if let Overlay::Processes(picker) = &mut self.overlay {
                    picker.selected = selected_index
                        .unwrap_or_else(|| picker.selected.min(process_count.saturating_sub(1)));
                }
                Ok(())
            }
            TurnUpdate::ProcessStopped { thread_id, result } => {
                if self.target.thread_id() != Some(thread_id) {
                    return Ok(());
                }
                match result {
                    Ok(()) => {
                        self.process_subscription = None;
                        let process_count = self.processes().len();
                        if let Overlay::Processes(picker) = &mut self.overlay {
                            picker.selected = picker.selected.min(process_count.saturating_sub(1));
                            picker.output_scroll = 0;
                            picker.state = crate::state::ProcessPickerState::Browsing;
                        }
                    }
                    Err(error) => {
                        if let Overlay::Processes(picker) = &mut self.overlay {
                            picker.state = crate::state::ProcessPickerState::Browsing;
                        }
                        self.error = Some(error);
                    }
                }
                Ok(())
            }
            TurnUpdate::ThreadRenamed { result } => {
                if matches!(
                    self.overlay,
                    Overlay::Operation(crate::state::OperationOverlay::RenamingThread)
                ) {
                    self.overlay = Overlay::None;
                }
                if let Err(error) = result {
                    self.error = Some(error);
                }
                Ok(())
            }
            TurnUpdate::ThreadDeleted { thread_id, result } => {
                match result {
                    Ok(()) => {
                        if self.target.thread_id() == Some(thread_id) {
                            self.reset_to_new_thread();
                        }
                        self.overlay = Overlay::None;
                    }
                    Err(error) => {
                        if let Overlay::ThreadPicker(picker) = &mut self.overlay {
                            picker.state = ThreadPickerState::Browsing;
                        }
                        self.error = Some(error);
                    }
                }
                Ok(())
            }
            TurnUpdate::ModelChanged { result } => {
                if matches!(
                    self.overlay,
                    Overlay::Operation(crate::state::OperationOverlay::ChangingModel)
                ) {
                    self.overlay = Overlay::None;
                }
                if let Err(error) = result {
                    self.error = Some(error);
                }
                Ok(())
            }
            TurnUpdate::CheckpointsLoaded { thread_id, result } => {
                if self.target.thread_id() != Some(thread_id) {
                    return Ok(());
                }
                if !matches!(self.overlay, Overlay::LoadingCheckpoints) {
                    return Ok(());
                }
                let checkpoint_subscription = match result {
                    Ok(subscription) => subscription,
                    Err(error) => {
                        self.overlay = Overlay::None;
                        self.error = Some(error);
                        return Ok(());
                    }
                };
                if checkpoint_subscription.is_none() {
                    self.checkpoint_subscription = None;
                    self.overlay = Overlay::NoCheckpoints;
                } else {
                    self.overlay = Overlay::None;
                    self.checkpoint_subscription = checkpoint_subscription.map(Into::into);
                    let subscription = self
                        .checkpoint_subscription
                        .as_ref()
                        .expect("checkpoint subscription was present");
                    let selected = subscription.state().metadata().id;
                    self.transcript
                        .replace_events(subscription.state().events());
                    self.target = Target::Thread {
                        id: thread_id,
                        view: ThreadView::Checkpoint {
                            picker: CheckpointPicker {
                                selected,
                                loading: false,
                            },
                        },
                    };
                    self.clear_selection();
                    self.reset_view();
                    self.view.focus = FocusPane::Checkpoints;
                }
                Ok(())
            }
            TurnUpdate::CheckpointLoaded {
                checkpoint_id,
                result,
            } => {
                let Some(picker) = self.target.checkpoint_picker() else {
                    return Ok(());
                };
                if picker.selected != checkpoint_id {
                    return Ok(());
                }
                let subscription = match result {
                    Ok(subscription) => subscription,
                    Err(error) => {
                        self.target
                            .checkpoint_picker_mut()
                            .expect("checkpoint picker was present")
                            .loading = false;
                        self.error = Some(error);
                        return Ok(());
                    }
                };
                let checkpoint = subscription.state().metadata();
                if self.target.thread_id() != Some(checkpoint.thread_id) {
                    return Ok(());
                }
                if checkpoint.id != checkpoint_id {
                    return Ok(());
                }
                self.transcript
                    .replace_events(subscription.state().events());
                self.checkpoint_subscription = Some(subscription.into());
                self.target
                    .checkpoint_picker_mut()
                    .expect("checkpoint picker was present")
                    .loading = false;
                self.clear_selection();
                self.reset_view();
                self.view.focus = FocusPane::Checkpoints;
                Ok(())
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
                    thread_id,
                    subscription,
                } = match result {
                    Ok(change) => change,
                    Err(error) => {
                        self.overlay = Overlay::None;
                        self.error = Some(error);
                        return Ok(());
                    }
                };
                self.transcript.rebuild(subscription.state());
                self.overlay = Overlay::None;
                self.thread_subscription = Some(subscription.into());
                self.sync_turn_interaction();
                self.target = Target::Thread {
                    id: thread_id,
                    view: ThreadView::Live,
                };
                self.process_subscription = None;
                self.clear_selection();
                self.reset_view();
                if let Some(draft) = draft {
                    self.message_input.set(draft);
                    self.view.focus = FocusPane::Input;
                }
                Ok(())
            }
        }
    }
}
