use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use atra_protocol::{ApprovalId, EventSequence, ProcessStatus, ThreadId};
use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc;

use super::{Activity, App, Target, ThreadView};
use crate::{
    history,
    input::{InputAction, InputBuffer},
    layout::SelectionPoint,
    runtime::{ApprovalDecision, Effect, HistoryOperation},
    state::{
        ApprovalState, FocusPane, HistoryAction, ModelPicker, ModelPickerStage, Overlay,
        ProcessPicker, ProcessPickerState, ThreadPicker, TurnState,
    },
    transcript::{sanitize, transcript_text},
};

impl App {
    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        effects: &mpsc::UnboundedSender<Effect>,
    ) -> Result<bool> {
        if matches!(
            self.turn,
            TurnState::Starting | TurnState::Running | TurnState::ResolvingApproval(_)
        ) && self.overlay.is_none()
            && key.code == KeyCode::Esc
        {
            self.cancel_turn(effects);
            return Ok(false);
        }
        if matches!(self.overlay, Overlay::Help) {
            let toggle = key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('/'));
            if toggle || matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                self.overlay = Overlay::None;
                self.activity = None;
            }
            return Ok(false);
        }

        if matches!(self.overlay, Overlay::Command) {
            let toggle = key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('/'));
            if toggle || key.code == KeyCode::Esc {
                self.overlay = Overlay::None;
                self.command_input.clear();
                self.activity = None;
                return Ok(false);
            }
            if matches!(
                self.command_input.handle_key(key, &self.word_segmenter),
                InputAction::Submit
            ) {
                self.overlay = Overlay::None;
                let command = self.command_input.take();
                return self.execute_command(&command, effects);
            }
            return Ok(false);
        }

        if let Some(approval) = self.turn.approval_mut() {
            if key.code == KeyCode::Esc && matches!(approval.state, ApprovalState::Pending) {
                self.cancel_turn(effects);
                return Ok(false);
            }
            match &mut approval.state {
                ApprovalState::EnteringDenyReason(reason) => match key.code {
                    KeyCode::Enter => {
                        let id = approval.id;
                        let reason = reason.take();
                        let reason = (!reason.trim().is_empty()).then_some(reason);
                        self.resolve_approval(id, false, reason, effects);
                    }
                    KeyCode::Esc => approval.state = ApprovalState::Pending,
                    _ => {
                        reason.handle_key(key, &self.word_segmenter);
                    }
                },
                ApprovalState::Pending => match key.code {
                    KeyCode::Char('y') => {
                        let id = approval.id;
                        self.resolve_approval(id, true, None, effects);
                    }
                    KeyCode::Char('n') => {
                        approval.state =
                            ApprovalState::EnteringDenyReason(InputBuffer::new(Vec::new(), false));
                    }
                    _ => {}
                },
            }
            return Ok(false);
        }

        if let Overlay::Processes(picker) = &mut self.overlay {
            if let ProcessPickerState::ConfirmingStop { runner, process_id } = &picker.state {
                match key.code {
                    KeyCode::Char('y') => {
                        self.activity = Some(Activity::Info(format!("Stopping {process_id}…")));
                        effects
                            .send(Effect::StopProcess {
                                endpoint: self.endpoint.clone(),
                                thread_id: self.target.thread_id().unwrap(),
                                runner: runner.clone(),
                                process_id: process_id.clone(),
                            })
                            .ok();
                        picker.state = ProcessPickerState::Browsing;
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        picker.state = ProcessPickerState::Browsing
                    }
                    _ => {}
                }
                return Ok(false);
            }
            let mut refresh_detail = false;
            match key.code {
                KeyCode::Up => {
                    let selected = picker.selected.saturating_sub(1);
                    refresh_detail = selected != picker.selected;
                    picker.selected = selected;
                }
                KeyCode::Down => {
                    let selected =
                        (picker.selected + 1).min(self.processes.len().saturating_sub(1));
                    refresh_detail = selected != picker.selected;
                    picker.selected = selected;
                }
                KeyCode::PageUp => picker.output_scroll = picker.output_scroll.saturating_add(10),
                KeyCode::PageDown => picker.output_scroll = picker.output_scroll.saturating_sub(10),
                KeyCode::Char('x')
                    if self.processes.get(picker.selected).is_some_and(|process| {
                        matches!(process.status, ProcessStatus::Running)
                    }) =>
                {
                    let process = &self.processes[picker.selected];
                    picker.state = ProcessPickerState::ConfirmingStop {
                        runner: process.runner.clone(),
                        process_id: process.process_id.clone(),
                    };
                }
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    self.activity = None;
                }
                _ => {}
            }
            if refresh_detail {
                if let Overlay::Processes(picker) = &mut self.overlay {
                    picker.detail = None;
                    picker.output_scroll = 0;
                }
                self.poll_processes(effects);
            }
            return Ok(false);
        }

        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('m') {
            self.open_model_picker()?;
            return Ok(false);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(key.code, KeyCode::Char('p') | KeyCode::Char('/')) && self.overlay.is_none()
            {
                self.command_input.clear();
                self.overlay = Overlay::Command;
                return Ok(false);
            }
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('g'))
                && self.overlay.is_none()
                && self.target.checkpoint_picker().is_none()
                && self.view.focus == FocusPane::Input
                && !self.message_input.value.trim().is_empty()
            {
                if self.message_input.value.starts_with('/') {
                    let command = self.message_input.take();
                    return self.execute_command(&command, effects);
                } else if !self.turn.is_running() {
                    self.send(effects)?;
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Char('r') if self.target.thread_id().is_some() => {
                    self.overlay = Overlay::Rename;
                    self.view.focus = FocusPane::Input;
                    let display_name = self
                        .threads
                        .iter()
                        .find(|thread| Some(thread.id) == self.target.thread_id())
                        .and_then(|thread| thread.display_name.clone())
                        .unwrap_or_default();
                    self.message_input.set(display_name);
                    self.activity = Some(Activity::Info(
                        "Enter saves the thread name · Esc cancels".to_owned(),
                    ));
                }
                KeyCode::Char('l') if self.login_required => {
                    self.activity = Some(Activity::Info(
                        "Complete Codex login in your browser…".to_owned(),
                    ));
                    effects
                        .send(Effect::Login {
                            endpoint: self.endpoint.clone(),
                        })
                        .ok();
                }
                KeyCode::Char('c')
                    if self
                        .selection_range()
                        .is_some_and(|(start, end)| start != end) =>
                {
                    self.copy_selection()?
                }
                KeyCode::Char('c') => {
                    if !self.message_input.value.is_empty() {
                        let input = self.message_input.take();
                        history::record(
                            &self.message_history_path,
                            &mut self.message_input,
                            input,
                        )?;
                    }
                }
                _ => {
                    self.message_input.handle_key(key, &self.word_segmenter);
                }
            }
            return Ok(false);
        }

        if key.code == KeyCode::Tab {
            self.cycle_focus(false);
            return Ok(false);
        }
        if key.code == KeyCode::BackTab {
            self.cycle_focus(true);
            return Ok(false);
        }

        if matches!(self.overlay, Overlay::Rename) {
            match key.code {
                KeyCode::Enter if !self.message_input.value.trim().is_empty() => {
                    self.rename(effects)?
                }
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    self.message_input.clear();
                    self.activity = None;
                }
                _ => {
                    self.message_input.handle_key(key, &self.word_segmenter);
                }
            }
            return Ok(false);
        }

        if let Overlay::ModelPicker(picker) = &mut self.overlay {
            match key.code {
                KeyCode::Up => {
                    if matches!(picker.stage, ModelPickerStage::Effort) {
                        picker.effort_index = picker.effort_index.saturating_sub(1);
                    } else {
                        picker.model_index = picker.model_index.saturating_sub(1);
                        let model = &picker.models[picker.model_index];
                        picker.effort_index = model
                            .supported_reasoning_efforts
                            .iter()
                            .position(|effort| effort == &model.default_reasoning_effort)
                            .unwrap_or(0);
                    }
                }
                KeyCode::Down => {
                    if matches!(picker.stage, ModelPickerStage::Effort) {
                        let count = picker.models[picker.model_index]
                            .supported_reasoning_efforts
                            .len();
                        picker.effort_index =
                            (picker.effort_index + 1).min(count.saturating_sub(1));
                    } else {
                        picker.model_index =
                            (picker.model_index + 1).min(picker.models.len().saturating_sub(1));
                        let model = &picker.models[picker.model_index];
                        picker.effort_index = model
                            .supported_reasoning_efforts
                            .iter()
                            .position(|effort| effort == &model.default_reasoning_effort)
                            .unwrap_or(0);
                    }
                }
                KeyCode::Enter if matches!(picker.stage, ModelPickerStage::Effort) => {
                    self.change_model(effects)?
                }
                KeyCode::Enter => {
                    picker.stage = ModelPickerStage::Effort;
                    self.activity = Some(Activity::Info(
                        "Select reasoning effort · Enter applies · Esc goes back".to_owned(),
                    ));
                }
                KeyCode::Esc => {
                    if matches!(picker.stage, ModelPickerStage::Effort) {
                        picker.stage = ModelPickerStage::Model;
                        self.activity = Some(Activity::Info(
                            "Select model · Enter chooses effort · Esc cancels".to_owned(),
                        ));
                    } else {
                        self.overlay = Overlay::None;
                        self.activity = None;
                    }
                }
                _ => {}
            }
            return Ok(false);
        }

        if let Overlay::ThreadPicker(picker) = &mut self.overlay {
            match key.code {
                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                KeyCode::Down => {
                    picker.selected = (picker.selected + 1).min(self.threads.len() - 1);
                }
                KeyCode::Enter => {
                    let thread_id = self.threads[picker.selected].id;
                    self.overlay = Overlay::None;
                    self.select_thread(thread_id, effects);
                }
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    self.activity = None;
                }
                _ => {}
            }
            return Ok(false);
        }

        if self.view.focus == FocusPane::Checkpoints
            && let Some(picker) = self.target.checkpoint_picker_mut()
        {
            match key.code {
                KeyCode::Up => {
                    let selected = picker.selected.saturating_sub(1);
                    if selected != picker.selected {
                        picker.selected = selected;
                        let checkpoint = picker.checkpoints[selected].clone();
                        self.activity = Some(Activity::Info("Loading checkpoint…".to_owned()));
                        effects
                            .send(Effect::LoadCheckpoint {
                                endpoint: self.endpoint.clone(),
                                checkpoint,
                            })
                            .ok();
                    }
                }
                KeyCode::Down => {
                    let selected =
                        (picker.selected + 1).min(picker.checkpoints.len().saturating_sub(1));
                    if selected != picker.selected {
                        picker.selected = selected;
                        let checkpoint = picker.checkpoints[selected].clone();
                        self.activity = Some(Activity::Info("Loading checkpoint…".to_owned()));
                        effects
                            .send(Effect::LoadCheckpoint {
                                endpoint: self.endpoint.clone(),
                                checkpoint,
                            })
                            .ok();
                    }
                }
                KeyCode::Esc => {}
                _ => return Ok(false),
            }
            if key.code != KeyCode::Esc {
                return Ok(false);
            }
        }

        if matches!(self.overlay, Overlay::HistoryConfirmation(_)) {
            match key.code {
                KeyCode::Char('y') => {
                    let Overlay::HistoryConfirmation(action) =
                        std::mem::replace(&mut self.overlay, Overlay::None)
                    else {
                        unreachable!()
                    };
                    self.run_history_action(action, effects)?;
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    self.activity = None;
                }
                _ => {}
            }
            return Ok(false);
        }

        if key.code == KeyCode::Esc
            && let Target::Thread {
                id: thread_id,
                view: ThreadView::Checkpoint { .. },
            } = &self.target
        {
            let thread_id = *thread_id;
            self.target = Target::Thread {
                id: thread_id,
                view: ThreadView::Live,
            };
            self.select_thread(thread_id, effects);
            return Ok(false);
        }

        if self.view.focus != FocusPane::Input {
            self.handle_pane_key(key);
            return Ok(false);
        }

        if key.code == KeyCode::Esc {
            self.clear_selection();
        } else {
            self.message_input.handle_key(key, &self.word_segmenter);
        }
        Ok(false)
    }

    fn execute_command(
        &mut self,
        command: &str,
        effects: &mpsc::UnboundedSender<Effect>,
    ) -> Result<bool> {
        let command = command.strip_prefix('/').unwrap_or(command);
        if !command.is_empty() {
            history::record(
                &self.command_history_path,
                &mut self.command_input,
                command.to_owned(),
            )?;
        }
        match command {
            "thread" => self.open_thread_picker(),
            "new" => self.start_new_thread(),
            "model" => self.open_model_picker()?,
            "checkpoint" => {
                self.run_command(|app| app.create_checkpoint(effects));
            }
            "checkpoints" => {
                self.run_command(|app| app.open_checkpoints(effects));
            }
            "fork" => {
                self.run_command(|app| app.fork_selected(effects));
            }
            "rewind" => {
                self.run_command(Self::confirm_rewind);
            }
            "restore" => {
                self.run_command(Self::confirm_restore);
            }
            "continue" => {
                self.run_command(|app| app.continue_thread(effects));
            }
            "compact" => {
                self.run_command(|app| app.compact_thread(effects));
            }
            "processes" => {
                self.run_command(|app| app.open_processes(effects));
            }
            "help" => {
                self.overlay = Overlay::Help;
                self.activity = Some(Activity::Info("Command help".to_owned()));
            }
            "exit" => return Ok(true),
            "" => self.activity = None,
            command => {
                self.activity = Some(Activity::Error(format!("Unknown command: /{command}")))
            }
        }
        Ok(false)
    }

    fn run_command(&mut self, command: impl FnOnce(&mut Self) -> Result<()>) {
        if let Err(error) = command(self) {
            self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
        }
    }

    fn open_processes(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        self.target.thread_id().context("no thread is selected")?;
        self.overlay = Overlay::Processes(ProcessPicker {
            selected: 0,
            detail: None,
            output_scroll: 0,
            state: ProcessPickerState::Browsing,
        });
        self.activity = Some(Activity::Info(
            "↑/↓ select · PageUp/PageDown output · x stop · Esc close".to_owned(),
        ));
        self.poll_processes(effects);
        Ok(())
    }

    pub(crate) fn poll_processes(&mut self, effects: &mpsc::UnboundedSender<Effect>) {
        let Some(thread_id) = self.target.thread_id() else {
            self.processes.clear();
            return;
        };
        if self.process_refresh_pending {
            return;
        }
        let selected = match &self.overlay {
            Overlay::Processes(picker) => self
                .processes
                .get(picker.selected)
                .map(|process| (process.runner.clone(), process.process_id.clone())),
            _ => None,
        };
        self.process_refresh_pending = true;
        effects
            .send(Effect::PollProcesses {
                endpoint: self.endpoint.clone(),
                thread_id,
                selected,
            })
            .ok();
    }

    fn start_new_thread(&mut self) {
        self.target = Target::New {
            model: self
                .models
                .first()
                .map(|model| (model.id.clone(), model.default_reasoning_effort.clone())),
        };
        self.processes.clear();
        self.transcript.clear();
        self.message_input.clear();
        self.overlay = Overlay::None;
        self.clear_selection();
        self.reset_view();
        self.metrics_stale = false;
        self.activity = Some(Activity::Info("New thread".to_owned()));
    }

    fn open_model_picker(&mut self) -> Result<()> {
        self.overlay = Overlay::None;
        if self.models.is_empty() {
            self.activity = Some(Activity::Info("No models are available".to_owned()));
            return Ok(());
        }
        let selected = self
            .threads
            .iter()
            .find(|thread| Some(thread.id) == self.target.thread_id())
            .map(|thread| (thread.model.as_str(), thread.reasoning_effort.as_str()))
            .or_else(|| {
                self.target
                    .new_thread_model()
                    .map(|(model, effort)| (model.as_str(), effort.as_str()))
            });
        let model_index = self
            .models
            .iter()
            .position(|model| selected.is_some_and(|(selected, _)| model.id == selected))
            .unwrap_or(0);
        let effort_index = self.models[model_index]
            .supported_reasoning_efforts
            .iter()
            .position(|effort| selected.is_some_and(|(_, selected)| effort == selected))
            .unwrap_or(0);
        self.overlay = Overlay::ModelPicker(ModelPicker {
            models: self.models.clone(),
            model_index,
            effort_index,
            stage: ModelPickerStage::Model,
        });
        self.activity = Some(Activity::Info(
            "Select model · Enter chooses effort · Esc cancels".to_owned(),
        ));
        Ok(())
    }

    fn open_thread_picker(&mut self) {
        self.overlay = Overlay::None;
        if self.threads.is_empty() {
            self.activity = Some(Activity::Info("No threads are available".to_owned()));
            return;
        }
        let selected = self
            .threads
            .iter()
            .position(|thread| Some(thread.id) == self.target.thread_id())
            .unwrap_or(0);
        self.overlay = Overlay::ThreadPicker(ThreadPicker { selected });
        self.activity = Some(Activity::Info(
            "Select thread · Enter switches · Esc cancels".to_owned(),
        ));
    }

    fn select_thread(&mut self, thread_id: ThreadId, effects: &mpsc::UnboundedSender<Effect>) {
        self.activity = Some(Activity::Info("Loading thread…".to_owned()));
        effects
            .send(Effect::SelectThread {
                endpoint: self.endpoint.clone(),
                thread_id,
            })
            .ok();
    }

    fn create_checkpoint(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.target.thread_id().context("no thread is selected")?;
        if self.target.checkpoint().is_some() {
            bail!("cannot checkpoint a checkpoint view");
        }
        if self.turn.is_running() {
            bail!("cannot checkpoint while a turn is running");
        }
        self.activity = Some(Activity::Info("Creating checkpoint…".to_owned()));
        effects
            .send(Effect::HistoryRequest {
                endpoint: self.endpoint.clone(),
                thread_id,
                draft: None,
                operation: HistoryOperation::CreateCheckpoint,
            })
            .ok();
        Ok(())
    }

    fn open_checkpoints(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.target.thread_id().context("no thread is selected")?;
        if self.turn.is_running() {
            bail!("cannot browse checkpoints while a turn is running");
        }
        self.activity = Some(Activity::Info("Loading checkpoints…".to_owned()));
        effects
            .send(Effect::LoadCheckpoints {
                endpoint: self.endpoint.clone(),
                thread_id,
            })
            .ok();
        Ok(())
    }

    fn selected_history_point(&self) -> Result<(EventSequence, Option<String>)> {
        let index = self
            .view
            .selected_item
            .context("select a user or assistant message in the transcript first")?;
        let entry = &self.transcript.entries[index];
        if !entry.is_assistant_message() && entry.user_message().is_none() {
            bail!("the selected transcript item is not a user or assistant message");
        }
        let sequence = entry
            .sequence
            .context("the selected transcript item has not been saved yet")?;
        Ok((sequence, entry.user_message().map(str::to_owned)))
    }

    fn fork_selected(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.target.thread_id().context("no thread is selected")?;
        if self.turn.is_running() {
            bail!("cannot fork while a turn is running");
        }
        let (sequence, draft) = self.selected_history_point()?;
        self.activity = Some(Activity::Info("Forking thread…".to_owned()));
        effects
            .send(Effect::HistoryRequest {
                endpoint: self.endpoint.clone(),
                thread_id,
                draft,
                operation: HistoryOperation::Fork {
                    checkpoint_id: self
                        .target
                        .checkpoint()
                        .as_ref()
                        .map(|checkpoint| checkpoint.id),
                    sequence,
                },
            })
            .ok();
        Ok(())
    }

    fn confirm_rewind(&mut self) -> Result<()> {
        if self.turn.is_running() {
            bail!("cannot rewind while a turn is running");
        }
        let (sequence, draft) = self.selected_history_point()?;
        self.overlay = Overlay::HistoryConfirmation(HistoryAction::Rewind {
            checkpoint_id: self
                .target
                .checkpoint()
                .as_ref()
                .map(|checkpoint| checkpoint.id),
            sequence,
            draft,
        });
        self.activity = Some(Activity::Info(
            "Rewind to selected turn? [y] Yes  [n] No".to_owned(),
        ));
        Ok(())
    }

    fn confirm_restore(&mut self) -> Result<()> {
        if self.turn.is_running() {
            bail!("cannot restore while a turn is running");
        }
        let checkpoint_id = self
            .target
            .checkpoint()
            .context("open a checkpoint with /checkpoints first")?
            .id;
        self.overlay = Overlay::HistoryConfirmation(HistoryAction::Restore { checkpoint_id });
        self.activity = Some(Activity::Info(
            "Restore this checkpoint? [y] Yes  [n] No".to_owned(),
        ));
        Ok(())
    }

    fn run_history_action(
        &mut self,
        action: HistoryAction,
        effects: &mpsc::UnboundedSender<Effect>,
    ) -> Result<()> {
        let thread_id = self.target.thread_id().context("no thread is selected")?;
        let (operation, draft) = match action {
            HistoryAction::Rewind {
                checkpoint_id,
                sequence,
                draft,
            } => (
                HistoryOperation::Rewind {
                    checkpoint_id,
                    sequence,
                },
                draft,
            ),
            HistoryAction::Restore { checkpoint_id } => {
                (HistoryOperation::Restore { checkpoint_id }, None)
            }
        };
        self.activity = Some(Activity::Info("Updating thread history…".to_owned()));
        effects
            .send(Effect::HistoryRequest {
                endpoint: self.endpoint.clone(),
                thread_id,
                draft,
                operation,
            })
            .ok();
        Ok(())
    }

    fn continue_thread(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.target.thread_id().context("no thread is selected")?;
        if self.target.checkpoint().is_some() {
            bail!("cannot continue a checkpoint view");
        }
        if self.turn.is_running() {
            bail!("a turn is already running");
        }
        self.turn = TurnState::Starting;
        self.activity = Some(Activity::Info("Starting turn… · Esc cancels".to_owned()));
        effects
            .send(Effect::ContinueTurn {
                endpoint: self.endpoint.clone(),
                thread_id,
            })
            .ok();
        Ok(())
    }

    fn compact_thread(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.target.thread_id().context("no thread is selected")?;
        if self.target.checkpoint().is_some() {
            bail!("cannot compact a checkpoint view");
        }
        if self.turn.is_running() {
            bail!("a turn is already running");
        }
        self.turn = TurnState::Starting;
        self.activity = Some(Activity::Info("Compacting… · Esc cancels".to_owned()));
        effects
            .send(Effect::CompactTurn {
                endpoint: self.endpoint.clone(),
                thread_id,
            })
            .ok();
        Ok(())
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if matches!(
            self.overlay,
            Overlay::Help | Overlay::ThreadPicker(_) | Overlay::HistoryConfirmation(_)
        ) {
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if self
                    .layout
                    .transcript_scrollbar_area
                    .contains((mouse.column, mouse.row).into()) =>
            {
                self.view.focus = FocusPane::Transcript;
                let position = mouse
                    .row
                    .saturating_sub(self.layout.transcript_scrollbar_area.y);
                if position == 0 {
                    self.view.transcript_scroll = self
                        .view
                        .transcript_scroll
                        .saturating_add(1)
                        .min(self.layout.transcript_max_scroll);
                } else if position
                    == self
                        .layout
                        .transcript_scrollbar_area
                        .height
                        .saturating_sub(1)
                {
                    self.view.transcript_scroll = self.view.transcript_scroll.saturating_sub(1);
                } else {
                    let track_position = position - 1;
                    let thumb_end = self
                        .layout
                        .transcript_scrollbar_thumb_start
                        .saturating_add(self.layout.transcript_scrollbar_thumb_len);
                    let drag_offset = if track_position
                        >= self.layout.transcript_scrollbar_thumb_start
                        && track_position < thumb_end
                    {
                        track_position - self.layout.transcript_scrollbar_thumb_start
                    } else {
                        self.layout.transcript_scrollbar_thumb_len / 2
                    };
                    self.layout.transcript_scrollbar_drag_offset = Some(drag_offset);
                    self.drag_transcript_scrollbar(track_position);
                }
                return Ok(());
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self.layout.transcript_scrollbar_drag_offset.is_some() =>
            {
                let track_position = mouse
                    .row
                    .saturating_sub(self.layout.transcript_scrollbar_area.y + 1)
                    .min(
                        self.layout
                            .transcript_scrollbar_area
                            .height
                            .saturating_sub(3),
                    );
                self.drag_transcript_scrollbar(track_position);
                return Ok(());
            }
            MouseEventKind::Up(MouseButton::Left)
                if self.layout.transcript_scrollbar_drag_offset.is_some() =>
            {
                self.layout.transcript_scrollbar_drag_offset = None;
                return Ok(());
            }
            _ => {}
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.view.transcript_scroll = self.view.transcript_scroll.saturating_add(3);
                self.view.focus = FocusPane::Transcript;
                return Ok(());
            }
            MouseEventKind::ScrollDown => {
                self.view.transcript_scroll = self.view.transcript_scroll.saturating_sub(3);
                self.view.focus = FocusPane::Transcript;
                return Ok(());
            }
            _ => {}
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.target.checkpoint_picker().is_none()
            && self
                .layout
                .input_area
                .contains((mouse.column, mouse.row).into())
        {
            self.view.focus = FocusPane::Input;
            return Ok(());
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some((index, _)) = self
                .layout
                .item_areas
                .iter()
                .find(|(_, area)| area.contains((mouse.column, mouse.row).into()))
        {
            let index = *index;
            if self.view.selected_item == Some(index)
                && self.transcript.entries[index].is_tool_result()
                && !self.view.expanded_tools.remove(&index)
            {
                self.view.expanded_tools.insert(index);
            }
            self.view.selected_item = Some(index);
            self.view.focus = FocusPane::Transcript;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self
                    .layout
                    .transcript_area
                    .contains((mouse.column, mouse.row).into())
                {
                    self.view.focus = FocusPane::Transcript;
                }
                self.view.selection_start = self.point_at(mouse.column, mouse.row);
                self.view.selection_end = self.view.selection_start;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.view.selection_start.is_some() => {
                self.view.selection_end = self.point_at(mouse.column, mouse.row);
            }
            _ => {}
        }
        Ok(())
    }

    fn drag_transcript_scrollbar(&mut self, track_position: u16) {
        let movable_height = self
            .layout
            .transcript_scrollbar_area
            .height
            .saturating_sub(2)
            .saturating_sub(self.layout.transcript_scrollbar_thumb_len);
        let thumb_start = track_position
            .saturating_sub(self.layout.transcript_scrollbar_drag_offset.unwrap())
            .min(movable_height);
        let scroll = if movable_height == 0 {
            0
        } else {
            self.layout
                .transcript_max_scroll
                .saturating_mul(usize::from(thumb_start))
                .saturating_add(usize::from(movable_height) / 2)
                / usize::from(movable_height)
        };
        self.view.transcript_scroll = self.layout.transcript_max_scroll.saturating_sub(scroll);
    }

    fn send(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let message = self.message_input.take();
        history::record(
            &self.message_history_path,
            &mut self.message_input,
            message.clone(),
        )?;
        self.turn = TurnState::Starting;
        self.activity = Some(Activity::Info("Starting turn… · Esc cancels".to_owned()));
        effects
            .send(Effect::SendTurn {
                endpoint: self.endpoint.clone(),
                thread_id: self.target.thread_id(),
                new_thread_model: match &mut self.target {
                    Target::New { model } => model.take(),
                    Target::Thread { .. } => None,
                },
                message,
            })
            .ok();
        Ok(())
    }

    fn cancel_turn(&mut self, effects: &mpsc::UnboundedSender<Effect>) {
        self.overlay = Overlay::None;
        let started = matches!(
            self.turn,
            TurnState::Running | TurnState::AwaitingApproval(_) | TurnState::ResolvingApproval(_)
        );
        self.turn = TurnState::Cancelling;
        self.activity = Some(Activity::Info("Cancelling…".to_owned()));
        if started {
            effects
                .send(Effect::CancelTurn {
                    endpoint: self.endpoint.clone(),
                    thread_id: self
                        .target
                        .thread_id()
                        .expect("active turn belongs to a thread"),
                })
                .ok();
        }
    }

    fn rename(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.target.thread_id().context("no thread is selected")?;
        let display_name = self.message_input.take();
        self.overlay = Overlay::None;
        self.activity = Some(Activity::Info("Renaming thread…".to_owned()));
        effects
            .send(Effect::RenameThread {
                endpoint: self.endpoint.clone(),
                thread_id,
                display_name,
            })
            .ok();
        Ok(())
    }

    fn change_model(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let picker = self
            .overlay
            .model_picker()
            .context("model picker is closed")?;
        let selected = &picker.models[picker.model_index];
        let model = selected.id.clone();
        let reasoning_effort = selected
            .supported_reasoning_efforts
            .get(picker.effort_index)
            .cloned()
            .unwrap_or_else(|| selected.default_reasoning_effort.clone());
        let Some(thread_id) = self.target.thread_id() else {
            self.target = Target::New {
                model: Some((model, reasoning_effort)),
            };
            self.overlay = Overlay::None;
            self.metrics_stale = true;
            self.activity = Some(Activity::Info("Model selected for new thread".to_owned()));
            return Ok(());
        };
        self.overlay = Overlay::None;
        self.activity = Some(Activity::Info("Changing thread model…".to_owned()));
        effects
            .send(Effect::ChangeModel {
                endpoint: self.endpoint.clone(),
                thread_id,
                model,
                reasoning_effort,
            })
            .ok();
        Ok(())
    }

    fn resolve_approval(
        &mut self,
        approval_id: ApprovalId,
        allowed: bool,
        reason: Option<String>,
        effects: &mpsc::UnboundedSender<Effect>,
    ) {
        let decision = if allowed {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny { reason }
        };
        self.transcript.set_pending_approval(None);
        let approval = match std::mem::replace(&mut self.turn, TurnState::Running) {
            TurnState::AwaitingApproval(approval) => approval,
            state => {
                self.turn = state;
                return;
            }
        };
        self.turn = TurnState::ResolvingApproval(approval);
        self.overlay = Overlay::None;
        self.activity = Some(Activity::Info(
            "Waiting for Atra Controller… · Esc cancels".to_owned(),
        ));
        effects
            .send(Effect::ResolveApproval {
                endpoint: self.endpoint.clone(),
                approval_id,
                decision,
            })
            .ok();
    }

    pub(super) fn restore_failed_approval(&mut self, approval_id: ApprovalId) {
        let approval = match std::mem::replace(&mut self.turn, TurnState::Running) {
            TurnState::ResolvingApproval(approval) if approval.id == approval_id => approval,
            state => {
                self.turn = state;
                return;
            }
        };
        self.transcript
            .set_pending_approval(approval.operation_index);
        self.turn = TurnState::AwaitingApproval(approval);
    }

    fn point_at(&self, column: u16, row: u16) -> Option<SelectionPoint> {
        let mapped = self
            .layout
            .transcript
            .rows
            .iter()
            .find(|line| line.y == row)?;
        let index = usize::from(column.saturating_sub(mapped.x));
        Some(SelectionPoint {
            offset: mapped.cells.get(index).copied().unwrap_or(mapped.end),
        })
    }

    pub(crate) fn selection_range(&self) -> Option<(usize, usize)> {
        let start = self.view.selection_start?.offset;
        let end = self.view.selection_end?.offset;
        Some(if start <= end {
            (start, end)
        } else {
            (end, start)
        })
    }

    fn copy_selection(&mut self) -> Result<()> {
        let Some((start, end)) = self.selection_range() else {
            return Ok(());
        };
        let text = transcript_text(&self.transcript.entries);
        let text = &text[start..end];
        write!(
            io::stdout(),
            "\x1b]52;c;{}\x07",
            STANDARD.encode(text.as_bytes())
        )
        .context("failed to write OSC 52 clipboard sequence")?;
        io::stdout().flush().context("failed to flush OSC 52")?;
        self.activity = Some(Activity::Info("Copied selection".to_owned()));
        Ok(())
    }

    pub(super) fn clear_selection(&mut self) {
        self.view.selection_start = None;
        self.view.selection_end = None;
    }

    pub(super) fn reset_view(&mut self) {
        self.view.transcript_scroll = 0;
        self.view.expanded_tools.clear();
        self.view.selected_item = None;
        self.view.focus = FocusPane::Input;
    }

    fn cycle_focus(&mut self, reverse: bool) {
        let panes: &[FocusPane] = if self.target.checkpoint_picker().is_some() {
            &[FocusPane::Checkpoints, FocusPane::Transcript]
        } else {
            &[FocusPane::Input, FocusPane::Transcript]
        };
        let current = panes
            .iter()
            .position(|pane| *pane == self.view.focus)
            .unwrap_or(0);
        let next = if reverse {
            current.checked_sub(1).unwrap_or(panes.len() - 1)
        } else {
            (current + 1) % panes.len()
        };
        self.view.focus = panes[next];
    }

    fn handle_pane_key(&mut self, key: KeyEvent) {
        match self.view.focus {
            FocusPane::Checkpoints => {}
            FocusPane::Transcript => match key.code {
                KeyCode::Home => self.view.transcript_scroll = self.layout.transcript_max_scroll,
                KeyCode::PageUp => {
                    self.view.transcript_scroll = self
                        .view
                        .transcript_scroll
                        .saturating_add(usize::from(self.layout.transcript_area.height))
                }
                KeyCode::PageDown => {
                    self.view.transcript_scroll = self
                        .view
                        .transcript_scroll
                        .saturating_sub(usize::from(self.layout.transcript_area.height))
                }
                KeyCode::Up => self.select_item(false),
                KeyCode::Down => self.select_item(true),
                KeyCode::Enter => {
                    if let Some(index) = self.view.selected_item
                        && self.transcript.entries[index].is_tool_result()
                        && !self.view.expanded_tools.remove(&index)
                    {
                        self.view.expanded_tools.insert(index);
                    }
                }
                KeyCode::End => self.view.transcript_scroll = 0,
                _ => {}
            },
            FocusPane::Input => {}
        }
    }

    fn select_item(&mut self, forward: bool) {
        if self.transcript.entries.is_empty() {
            self.view.selected_item = None;
            return;
        }
        let next = match (self.view.selected_item, forward) {
            (Some(index), true) => (index + 1).min(self.transcript.entries.len() - 1),
            (Some(index), false) => index.saturating_sub(1),
            (None, true) => 0,
            (None, false) => self.transcript.entries.len() - 1,
        };
        self.view.selected_item = Some(next);
        let scroll = self
            .layout
            .transcript_item_ranges
            .iter()
            .find_map(|(index, rows)| (*index == next).then_some(rows.start));
        if let Some(item_start) = scroll {
            let viewport_start = self
                .layout
                .transcript_max_scroll
                .saturating_sub(self.view.transcript_scroll);
            let viewport_end =
                viewport_start + usize::from(self.layout.transcript_area.height.saturating_sub(2));
            if item_start < viewport_start || item_start >= viewport_end {
                self.view.transcript_scroll = self
                    .layout
                    .transcript_max_scroll
                    .saturating_sub(item_start.min(self.layout.transcript_max_scroll));
            }
        }
    }
}
