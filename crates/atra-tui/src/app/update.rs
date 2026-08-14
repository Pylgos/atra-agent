use anyhow::Result;
use atra_protocol::{ControllerChange, ThreadChange, TurnPhase};

use super::{Activity, App, HistoryChange, Target, ThreadView, TurnUpdate};
use crate::{
    state::{CheckpointPicker, FocusPane, Overlay, ThreadPickerState, TurnState},
    transcript::sanitize,
};

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
        self.turn = match std::mem::take(&mut self.turn) {
            TurnState::Starting if active => TurnState::Idle,
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
                self.reset_turn_interaction();
                self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
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
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                Ok(())
            }
            TurnUpdate::CancelCompleted { thread_id, result } => {
                if self.target.thread_id() == Some(thread_id) {
                    match result {
                        Ok(()) => {}
                        Err(error) => {
                            self.reset_turn_interaction();
                            self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                        }
                    }
                }
                Ok(())
            }
            TurnUpdate::LoginCompleted(result) => {
                match result {
                    Ok(()) => {
                        self.login_required = false;
                        self.activity = Some(Activity::Info("Codex login complete".to_owned()));
                    }
                    Err(error) => {
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                Ok(())
            }
            TurnUpdate::ThreadSelected { thread_id, result } => {
                let subscription = result?;
                self.transcript.rebuild(subscription.state());
                self.thread_subscription = Some(subscription.into());
                self.reset_turn_interaction();
                self.target = Target::Thread {
                    id: thread_id,
                    view: ThreadView::Live,
                };
                self.process_subscription = None;
                self.overlay = Overlay::None;
                self.clear_selection();
                self.reset_view();
                self.activity = Some(Activity::Info("Thread selected".to_owned()));
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
                            self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
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
            TurnUpdate::ProcessStopped {
                thread_id,
                process_id,
                result,
            } => {
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
                        }
                        self.activity = Some(Activity::Info(format!("Stopped {process_id}")));
                    }
                    Err(error) => {
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                Ok(())
            }
            TurnUpdate::ThreadRenamed { result } => {
                match result {
                    Ok(()) => {
                        self.activity = Some(Activity::Info("Thread renamed".to_owned()));
                    }
                    Err(error) => {
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
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
                        self.activity = Some(Activity::Info("Thread deleted".to_owned()));
                    }
                    Err(error) => {
                        if let Overlay::ThreadPicker(picker) = &mut self.overlay {
                            picker.state = ThreadPickerState::Browsing;
                        }
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                Ok(())
            }
            TurnUpdate::ModelChanged { result } => {
                match result {
                    Ok(()) => {
                        self.activity = Some(Activity::Info("Thread model changed".to_owned()));
                    }
                    Err(error) => {
                        self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                    }
                }
                Ok(())
            }
            TurnUpdate::CheckpointsLoaded { thread_id, result } => {
                if self.target.thread_id() != Some(thread_id) {
                    return Ok(());
                }
                let checkpoint_subscription = result?;
                if checkpoint_subscription.is_none() {
                    self.checkpoint_subscription = None;
                    self.activity = Some(Activity::Info("No checkpoints are available".to_owned()));
                } else {
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
                            picker: CheckpointPicker { selected },
                        },
                    };
                    self.clear_selection();
                    self.reset_view();
                    self.view.focus = FocusPane::Checkpoints;
                    self.activity = Some(Activity::Info(
                        "Browse checkpoints · Tab switches pane · Esc returns".to_owned(),
                    ));
                }
                Ok(())
            }
            TurnUpdate::CheckpointLoaded(result) => {
                let subscription = result?;
                let checkpoint = subscription.state().metadata();
                if self.target.thread_id() != Some(checkpoint.thread_id) {
                    return Ok(());
                }
                if let Some(picker) = self.target.checkpoint_picker()
                    && picker.selected != checkpoint.id
                {
                    return Ok(());
                }
                self.transcript
                    .replace_events(subscription.state().events());
                self.checkpoint_subscription = Some(subscription.into());
                self.clear_selection();
                self.reset_view();
                if self.target.checkpoint_picker().is_some() {
                    self.view.focus = FocusPane::Checkpoints;
                }
                self.activity = Some(Activity::Info(
                    "Browse checkpoints · Tab switches pane · Esc returns".to_owned(),
                ));
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
                    message,
                    thread_id,
                    subscription,
                } = result?;
                self.transcript.rebuild(subscription.state());
                self.thread_subscription = Some(subscription.into());
                self.reset_turn_interaction();
                self.target = Target::Thread {
                    id: thread_id,
                    view: ThreadView::Live,
                };
                self.process_subscription = None;
                self.overlay = Overlay::None;
                self.clear_selection();
                self.reset_view();
                if let Some(draft) = draft {
                    self.message_input.set(draft);
                    self.view.focus = FocusPane::Input;
                }
                self.activity = Some(Activity::Info(message));
                Ok(())
            }
        }
    }
}
