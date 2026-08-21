use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use atra_protocol::{EventSequence, InteractionId, ProcessStatus, ThreadId, TurnPhase};
use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc;

use super::{App, Target, ThreadView};
use crate::{
    history,
    input::{InputAction, InputBuffer},
    layout::SelectionPoint,
    runtime::{ApprovalDecision, Effect, HistoryOperation},
    state::{
        FocusPane, HistoryAction, ModelPicker, ModelPickerStage, OperationOverlay, Overlay,
        ProcessPicker, ProcessPickerState, QuestionFormMode, ThreadPicker, ThreadPickerState,
        TurnState,
    },
    text::offset_at_position,
    transcript::transcript_text,
};

fn input_offset_at(
    value: &str,
    area: ratatui::layout::Rect,
    scroll: (u16, u16),
    column: u16,
    row: u16,
) -> usize {
    let visual_row = usize::from(row.saturating_sub(area.y).saturating_add(scroll.0));
    let visual_column = usize::from(column.saturating_sub(area.x).saturating_add(scroll.1));
    offset_at_position(value, visual_row, visual_column)
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

fn is_command_toggle(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('/'))
}

impl App {
    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        effects: &mpsc::UnboundedSender<Effect>,
    ) -> Result<bool> {
        if self.error.is_some() {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                self.error = None;
            }
            return Ok(false);
        }
        if self.overlay.is_none() && is_command_toggle(key) {
            self.command_input.clear();
            self.overlay = Overlay::Command;
            return Ok(false);
        }
        if matches!(self.overlay, Overlay::Command) {
            if is_ctrl_c(key) {
                self.handle_ctrl_c(effects)?;
                return Ok(false);
            }
            if is_command_toggle(key) || key.code == KeyCode::Esc {
                self.overlay = Overlay::None;
                self.command_input.clear();
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
        if self.overlay.is_none() && self.handle_question_key(key, effects)? {
            return Ok(false);
        }
        if self.turn_is_running()
            && self.pending_approval().is_none()
            && self.overlay.is_none()
            && key.code == KeyCode::Esc
        {
            self.cancel_turn(effects);
            return Ok(false);
        }
        if matches!(self.overlay, Overlay::Help) {
            if is_command_toggle(key) || matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                self.overlay = Overlay::None;
            }
            return Ok(false);
        }
        if matches!(
            self.overlay,
            Overlay::LoadingCheckpoints | Overlay::NoCheckpoints | Overlay::Operation(_)
        ) {
            if !matches!(self.overlay, Overlay::Operation(_))
                && matches!(key.code, KeyCode::Enter | KeyCode::Esc)
            {
                self.overlay = Overlay::None;
            }
            return Ok(false);
        }

        if self.overlay.is_none()
            && (self.pending_approval().is_some()
                || matches!(self.turn, TurnState::EnteringDenyReason { .. }))
            && is_ctrl_c(key)
        {
            self.handle_ctrl_c(effects)?;
            return Ok(false);
        }

        if self.overlay.is_none()
            && let TurnState::EnteringDenyReason {
                approval_id,
                reason,
            } = &mut self.turn
        {
            match key.code {
                KeyCode::Enter => {
                    let approval_id = *approval_id;
                    let reason = reason.take();
                    let reason = (!reason.trim().is_empty()).then_some(reason);
                    self.resolve_approval(approval_id, false, reason, effects);
                }
                KeyCode::Esc => self.sync_turn_interaction(),
                _ => {
                    reason.handle_key(key, &self.word_segmenter);
                }
            }
            return Ok(false);
        }

        if self.overlay.is_none()
            && let Some(approval_id) = self.pending_approval().map(|approval| approval.id())
        {
            match key.code {
                KeyCode::Esc => self.cancel_turn(effects),
                KeyCode::Char('y') => {
                    self.resolve_approval(approval_id, true, None, effects);
                }
                KeyCode::Char('n') => {
                    self.turn = TurnState::EnteringDenyReason {
                        approval_id,
                        reason: InputBuffer::new(Vec::new(), false),
                    };
                }
                _ => {}
            }
            return Ok(false);
        }

        let processes = self
            .processes()
            .iter()
            .map(|process| {
                (
                    process.locator().runner().to_owned(),
                    process.locator().process_id().clone(),
                    process.status().clone(),
                )
            })
            .collect::<Vec<_>>();
        if let Overlay::Processes(picker) = &mut self.overlay {
            if let ProcessPickerState::ConfirmingStop { runner, process_id } = &picker.state {
                match key.code {
                    KeyCode::Char('y') => {
                        let process_id = process_id.clone();
                        effects
                            .send(Effect::StopProcess {
                                endpoint: self.endpoint.clone(),
                                thread_id: self.target.thread_id().unwrap(),
                                runner: runner.clone(),
                                process_id: process_id.clone(),
                            })
                            .ok();
                        picker.state = ProcessPickerState::Stopping { process_id };
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        picker.state = ProcessPickerState::Browsing
                    }
                    _ => {}
                }
                return Ok(false);
            }
            if matches!(picker.state, ProcessPickerState::Stopping { .. }) {
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
                    let selected = (picker.selected + 1).min(processes.len().saturating_sub(1));
                    refresh_detail = selected != picker.selected;
                    picker.selected = selected;
                }
                KeyCode::PageUp => picker.output_scroll = picker.output_scroll.saturating_add(10),
                KeyCode::PageDown => picker.output_scroll = picker.output_scroll.saturating_sub(10),
                KeyCode::Char('x')
                    if processes
                        .get(picker.selected)
                        .is_some_and(|process| matches!(process.2, ProcessStatus::Running)) =>
                {
                    let process = &processes[picker.selected];
                    picker.state = ProcessPickerState::ConfirmingStop {
                        runner: process.0.clone(),
                        process_id: process.1.clone(),
                    };
                }
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                }
                _ => {}
            }
            if refresh_detail {
                self.process_subscription = None;
                if let Overlay::Processes(picker) = &mut self.overlay {
                    picker.output_scroll = 0;
                }
                self.select_process(effects);
            }
            return Ok(false);
        }

        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('m') {
            self.open_model_picker()?;
            return Ok(false);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('c') {
                self.handle_ctrl_c(effects)?;
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
                } else if !self.turn_is_running() {
                    self.send(effects)?;
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Char('r') if self.target.thread_id().is_some() => {
                    self.overlay = Overlay::Rename;
                    self.view.focus = FocusPane::Input;
                    let display_name = self
                        .threads()
                        .iter()
                        .find(|thread| Some(thread.id) == self.target.thread_id())
                        .and_then(|thread| thread.display_name.clone())
                        .unwrap_or_default();
                    self.message_input.set(display_name);
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
                }
                _ => {
                    self.message_input.handle_key(key, &self.word_segmenter);
                }
            }
            return Ok(false);
        }

        if let Overlay::ModelPicker(picker) = &mut self.overlay {
            match key.code {
                KeyCode::Up => match picker.stage {
                    ModelPickerStage::Provider => {
                        let provider_index = picker.provider_index.saturating_sub(1);
                        if provider_index != picker.provider_index {
                            picker.select_provider(provider_index);
                        }
                    }
                    ModelPickerStage::Model => {
                        let visible = picker.visible_model_indices();
                        if let Some(position) = visible
                            .iter()
                            .position(|index| *index == picker.model_index)
                        {
                            picker.select_model(visible[position.saturating_sub(1)]);
                        }
                    }
                    ModelPickerStage::Effort => {
                        picker.effort_index = picker.effort_index.saturating_sub(1);
                    }
                },
                KeyCode::Down => match picker.stage {
                    ModelPickerStage::Provider => {
                        let last = picker.providers().len().saturating_sub(1);
                        let provider_index = (picker.provider_index + 1).min(last);
                        if provider_index != picker.provider_index {
                            picker.select_provider(provider_index);
                        }
                    }
                    ModelPickerStage::Model => {
                        let visible = picker.visible_model_indices();
                        if let Some(position) = visible
                            .iter()
                            .position(|index| *index == picker.model_index)
                        {
                            let position = (position + 1).min(visible.len().saturating_sub(1));
                            picker.select_model(visible[position]);
                        }
                    }
                    ModelPickerStage::Effort => {
                        let count = picker
                            .selected_model()
                            .map_or(0, |model| model.supported_reasoning_efforts.len());
                        picker.effort_index =
                            (picker.effort_index + 1).min(count.saturating_sub(1));
                    }
                },
                KeyCode::Enter if matches!(picker.stage, ModelPickerStage::Effort) => {
                    self.change_model(effects)?
                }
                KeyCode::Enter if matches!(picker.stage, ModelPickerStage::Provider) => {
                    picker.stage = ModelPickerStage::Model;
                }
                KeyCode::Enter if picker.visible_model_indices().contains(&picker.model_index) => {
                    picker.stage = ModelPickerStage::Effort;
                }
                KeyCode::Esc => match picker.stage {
                    ModelPickerStage::Effort => {
                        picker.stage = ModelPickerStage::Model;
                    }
                    ModelPickerStage::Model if !picker.query.is_empty() => {
                        picker.query.clear();
                        picker.select_first_visible_model();
                    }
                    ModelPickerStage::Model => {
                        picker.stage = ModelPickerStage::Provider;
                    }
                    ModelPickerStage::Provider => {
                        self.overlay = Overlay::None;
                    }
                },
                KeyCode::Backspace if matches!(picker.stage, ModelPickerStage::Model) => {
                    picker.query.pop();
                    picker.select_first_visible_model();
                }
                KeyCode::Char('u')
                    if matches!(picker.stage, ModelPickerStage::Model)
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    picker.query.clear();
                    picker.select_first_visible_model();
                }
                KeyCode::Char(character)
                    if matches!(picker.stage, ModelPickerStage::Model)
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    picker.query.push(character);
                    picker.select_first_visible_model();
                }
                _ => {}
            }
            return Ok(false);
        }

        let all_threads = self.threads().to_vec();
        let thread_ids = match &self.overlay {
            Overlay::ThreadPicker(picker) => {
                crate::state::visible_threads(&all_threads, &picker.collapsed)
                    .into_iter()
                    .map(|(thread, _)| thread.id)
                    .collect::<Vec<_>>()
            }
            _ => Vec::new(),
        };
        if let Overlay::ThreadPicker(picker) = &mut self.overlay {
            picker.selected = picker.selected.min(thread_ids.len().saturating_sub(1));
            if matches!(
                picker.state,
                ThreadPickerState::Selecting | ThreadPickerState::Deleting
            ) {
                return Ok(false);
            }
            if matches!(picker.state, ThreadPickerState::ConfirmingDelete) {
                match key.code {
                    KeyCode::Char('y') => {
                        let Some(&thread_id) = thread_ids.get(picker.selected) else {
                            picker.state = ThreadPickerState::Browsing;
                            return Ok(false);
                        };
                        let current_is_in_subtree =
                            self.target.thread_id().is_some_and(|current| {
                                let mut candidate = Some(current);
                                while let Some(id) = candidate {
                                    if id == thread_id {
                                        return true;
                                    }
                                    candidate = all_threads
                                        .iter()
                                        .find(|thread| thread.id == id)
                                        .and_then(|thread| thread.parent_thread_id);
                                }
                                false
                            });
                        let select_after = current_is_in_subtree
                            .then(|| {
                                all_threads
                                    .iter()
                                    .find(|thread| thread.id == thread_id)
                                    .and_then(|thread| thread.parent_thread_id)
                            })
                            .flatten();
                        effects
                            .send(Effect::DeleteThread {
                                endpoint: self.endpoint.clone(),
                                thread_id,
                                recursive: all_threads
                                    .iter()
                                    .any(|thread| thread.parent_thread_id == Some(thread_id)),
                                selected_subtree: current_is_in_subtree,
                                select_after,
                            })
                            .ok();
                        picker.state = ThreadPickerState::Deleting;
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        picker.state = ThreadPickerState::Browsing;
                    }
                    _ => {}
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                KeyCode::Down => {
                    picker.selected = (picker.selected + 1).min(thread_ids.len().saturating_sub(1));
                }
                KeyCode::Right => {
                    if let Some(id) = thread_ids.get(picker.selected) {
                        picker.collapsed.remove(id);
                    }
                }
                KeyCode::Left => {
                    if let Some(&id) = thread_ids.get(picker.selected) {
                        let has_children = all_threads
                            .iter()
                            .any(|thread| thread.parent_thread_id == Some(id));
                        if !has_children || picker.collapsed.contains(&id) {
                            if let Some(parent) = all_threads
                                .iter()
                                .find(|thread| thread.id == id)
                                .and_then(|thread| thread.parent_thread_id)
                                && let Some(index) =
                                    thread_ids.iter().position(|candidate| *candidate == parent)
                            {
                                picker.selected = index;
                            }
                        } else {
                            picker.collapsed.insert(id);
                        }
                    }
                }
                KeyCode::Char('x') if !thread_ids.is_empty() => {
                    picker.state = ThreadPickerState::ConfirmingDelete;
                }
                KeyCode::Enter if thread_ids.get(picker.selected).is_some() => {
                    let thread_id = thread_ids[picker.selected];
                    picker.state = ThreadPickerState::Selecting;
                    self.select_thread(thread_id, effects);
                }
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                }
                _ => {}
            }
            return Ok(false);
        }

        if self.view.focus == FocusPane::Checkpoints
            && let Some(picker) = self.target.checkpoint_picker_mut()
        {
            let mut selected = None;
            match key.code {
                KeyCode::Up => {
                    selected = Some(-1_isize);
                }
                KeyCode::Down => {
                    selected = Some(1);
                }
                KeyCode::Esc => {}
                _ => return Ok(false),
            }
            let current = picker.selected;
            if let Some(offset) = selected {
                let checkpoints = self.checkpoints();
                let current_index = checkpoints
                    .iter()
                    .position(|checkpoint| checkpoint.id == current)
                    .unwrap_or_default();
                let selected_index = current_index
                    .saturating_add_signed(offset)
                    .min(checkpoints.len().saturating_sub(1));
                if let Some(checkpoint) = checkpoints.get(selected_index)
                    && checkpoint.id != current
                {
                    let checkpoint_id = checkpoint.id;
                    self.target
                        .checkpoint_picker_mut()
                        .expect("checkpoint picker was present")
                        .selected = checkpoint_id;
                    self.target
                        .checkpoint_picker_mut()
                        .expect("checkpoint picker was present")
                        .loading = true;
                    effects
                        .send(Effect::LoadCheckpoint {
                            endpoint: self.endpoint.clone(),
                            checkpoint_id,
                        })
                        .ok();
                }
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

    fn handle_question_key(
        &mut self,
        key: KeyEvent,
        effects: &mpsc::UnboundedSender<Effect>,
    ) -> Result<bool> {
        let TurnState::AnsweringQuestions(form) = &mut self.turn else {
            return Ok(false);
        };
        if form.mode == QuestionFormMode::Submitting {
            return Ok(true);
        }
        let mut submit = false;
        let mut cancel = false;
        let question_count = form.request.questions.len();
        match form.mode {
            QuestionFormMode::Note => {
                let confirm = key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Enter | KeyCode::Char('g'));
                if confirm {
                    form.mode = QuestionFormMode::Normal;
                    if form.current + 1 == question_count {
                        form.mode = QuestionFormMode::Confirm;
                        form.scroll = 0;
                    } else {
                        form.current += 1;
                    }
                } else {
                    match key.code {
                        KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => {
                            form.mode = QuestionFormMode::Normal;
                        }
                        _ => {
                            form.drafts[form.current]
                                .note
                                .handle_key(key, &self.word_segmenter);
                        }
                    }
                }
            }
            QuestionFormMode::Normal => match key.code {
                KeyCode::Up => {
                    form.drafts[form.current].selected =
                        form.drafts[form.current].selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    let last = form.request.questions[form.current].options.len();
                    form.drafts[form.current].selected =
                        (form.drafts[form.current].selected + 1).min(last);
                }
                KeyCode::Tab | KeyCode::BackTab => form.mode = QuestionFormMode::Note,
                KeyCode::Left => {
                    form.current = form.current.saturating_sub(1);
                }
                KeyCode::Enter | KeyCode::Right => {
                    if form.current + 1 == question_count {
                        form.mode = QuestionFormMode::Confirm;
                        form.scroll = 0;
                    } else {
                        form.current += 1;
                    }
                }
                KeyCode::Esc => cancel = true,
                _ => {}
            },
            QuestionFormMode::Confirm => match key.code {
                KeyCode::Enter | KeyCode::Right => submit = true,
                KeyCode::Left | KeyCode::Esc => {
                    form.current = question_count.saturating_sub(1);
                    form.mode = QuestionFormMode::Normal;
                }
                KeyCode::Up => form.scroll = form.scroll.saturating_sub(1),
                KeyCode::Down => form.scroll = form.scroll.saturating_add(1),
                _ => {}
            },
            QuestionFormMode::Submitting => unreachable!(),
        }
        if cancel {
            self.cancel_turn(effects);
        } else if submit {
            let request_id = form.id();
            let answers = form.answers();
            form.mode = QuestionFormMode::Submitting;
            effects
                .send(Effect::ResolveQuestion {
                    endpoint: self.endpoint.clone(),
                    request_id,
                    answers,
                })
                .ok();
        }
        Ok(true)
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
            }
            "exit" => return Ok(true),
            "" => {}
            command => self.error = Some(anyhow::anyhow!("unknown command: /{command}")),
        }
        Ok(false)
    }

    fn run_command(&mut self, command: impl FnOnce(&mut Self) -> Result<()>) {
        if let Err(error) = command(self) {
            self.error = Some(error);
        }
    }

    fn open_processes(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        self.target.thread_id().context("no thread is selected")?;
        self.overlay = Overlay::Processes(ProcessPicker {
            selected: 0,
            output_scroll: 0,
            state: ProcessPickerState::Browsing,
        });
        self.select_process(effects);
        Ok(())
    }

    pub(crate) fn select_process(&mut self, effects: &mpsc::UnboundedSender<Effect>) {
        let Some(thread_id) = self.target.thread_id() else {
            self.process_subscription = None;
            return;
        };
        if self.process_selection_pending {
            return;
        }
        let selected = match &self.overlay {
            Overlay::Processes(picker) => self.processes().get(picker.selected).map(|process| {
                (
                    process.locator().runner().to_owned(),
                    process.locator().process_id().clone(),
                )
            }),
            _ => None,
        };
        self.process_selection_pending = true;
        effects
            .send(Effect::SelectProcess {
                endpoint: self.endpoint.clone(),
                thread_id,
                selected,
            })
            .ok();
    }

    fn start_new_thread(&mut self) {
        self.reset_to_new_thread();
        self.overlay = Overlay::None;
    }

    pub(crate) fn reset_to_new_thread(&mut self) {
        let model = self.models().first().map(|model| {
            (
                model.provider.clone(),
                model.id.clone(),
                model.default_reasoning_effort.clone(),
            )
        });
        self.target = Target::New { model };
        self.thread_subscription = None;
        self.checkpoint_subscription = None;
        self.process_subscription = None;
        self.turn = TurnState::Idle;
        self.transcript.clear();
        self.message_input.clear();
        self.clear_selection();
        self.reset_view();
    }

    fn open_model_picker(&mut self) -> Result<()> {
        self.overlay = Overlay::None;
        let models = self.models();
        let selected = self
            .threads()
            .iter()
            .find(|thread| Some(thread.id) == self.target.thread_id())
            .map(|thread| {
                (
                    thread.provider.as_str(),
                    thread.model.as_str(),
                    thread.reasoning_effort.as_str(),
                )
            })
            .or_else(|| {
                self.target
                    .new_thread_model()
                    .map(|(provider, model, effort)| {
                        (provider.as_str(), model.as_str(), effort.as_str())
                    })
            });
        let model_index = models
            .iter()
            .position(|model| {
                selected.is_some_and(|(provider, selected, _)| {
                    model.provider == provider && model.id == selected
                })
            })
            .unwrap_or(0);
        let effort_index = models
            .get(model_index)
            .and_then(|model| {
                model
                    .supported_reasoning_efforts
                    .iter()
                    .position(|effort| selected.is_some_and(|(_, _, selected)| effort == selected))
            })
            .unwrap_or(0);
        let mut picker = ModelPicker {
            models,
            provider_index: 0,
            model_index,
            effort_index,
            query: String::new(),
            stage: ModelPickerStage::Provider,
        };
        if let Some(model) = picker.models.get(model_index) {
            picker.provider_index = picker
                .providers()
                .iter()
                .position(|provider| *provider == model.provider)
                .unwrap_or(0);
        }
        self.overlay = Overlay::ModelPicker(picker);
        Ok(())
    }

    fn open_thread_picker(&mut self) {
        self.overlay = Overlay::None;
        let current = self.target.thread_id();
        let threads = self.threads();
        let mut expanded = std::collections::HashSet::new();
        let mut parent = current
            .and_then(|id| threads.iter().find(|thread| thread.id == id))
            .and_then(|thread| thread.parent_thread_id);
        while let Some(id) = parent {
            expanded.insert(id);
            parent = threads
                .iter()
                .find(|thread| thread.id == id)
                .and_then(|thread| thread.parent_thread_id);
        }
        let collapsed = threads
            .iter()
            .filter(|thread| thread.parent_thread_id.is_none() && !expanded.contains(&thread.id))
            .map(|thread| thread.id)
            .collect();
        let visible = crate::state::visible_threads(threads, &collapsed);
        let selected = visible
            .iter()
            .position(|(thread, _)| Some(thread.id) == current)
            .unwrap_or(0);
        self.overlay = Overlay::ThreadPicker(ThreadPicker {
            selected,
            state: ThreadPickerState::Browsing,
            collapsed,
        });
    }

    fn select_thread(&mut self, thread_id: ThreadId, effects: &mpsc::UnboundedSender<Effect>) {
        effects
            .send(Effect::SelectThread {
                endpoint: self.endpoint.clone(),
                thread_id,
            })
            .ok();
    }

    fn create_checkpoint(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.target.thread_id().context("no thread is selected")?;
        if self.checkpoint().is_some() {
            bail!("cannot checkpoint a checkpoint view");
        }
        if self.turn_is_running() {
            bail!("cannot checkpoint while a turn is running");
        }
        self.overlay = Overlay::Operation(OperationOverlay::CreatingCheckpoint);
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
        if self.turn_is_running() {
            bail!("cannot browse checkpoints while a turn is running");
        }
        let checkpoint_id = self.checkpoints().first().map(|checkpoint| checkpoint.id);
        self.overlay = Overlay::LoadingCheckpoints;
        effects
            .send(Effect::LoadCheckpoints {
                endpoint: self.endpoint.clone(),
                thread_id,
                checkpoint_id,
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
        if self.turn_is_running() {
            bail!("cannot fork while a turn is running");
        }
        let (sequence, draft) = self.selected_history_point()?;
        self.overlay = Overlay::Operation(OperationOverlay::ForkingThread);
        effects
            .send(Effect::HistoryRequest {
                endpoint: self.endpoint.clone(),
                thread_id,
                draft,
                operation: HistoryOperation::Fork {
                    checkpoint_id: self.checkpoint().map(|checkpoint| checkpoint.id),
                    sequence,
                },
            })
            .ok();
        Ok(())
    }

    fn confirm_rewind(&mut self) -> Result<()> {
        if self.turn_is_running() {
            bail!("cannot rewind while a turn is running");
        }
        let (sequence, draft) = self.selected_history_point()?;
        self.overlay = Overlay::HistoryConfirmation(HistoryAction::Rewind {
            checkpoint_id: self.checkpoint().map(|checkpoint| checkpoint.id),
            sequence,
            draft,
        });
        Ok(())
    }

    fn confirm_restore(&mut self) -> Result<()> {
        if self.turn_is_running() {
            bail!("cannot restore while a turn is running");
        }
        let checkpoint_id = self
            .checkpoint()
            .context("open a checkpoint with /checkpoints first")?
            .id;
        self.overlay = Overlay::HistoryConfirmation(HistoryAction::Restore { checkpoint_id });
        Ok(())
    }

    fn run_history_action(
        &mut self,
        action: HistoryAction,
        effects: &mpsc::UnboundedSender<Effect>,
    ) -> Result<()> {
        let thread_id = self.target.thread_id().context("no thread is selected")?;
        let (operation, draft, progress) = match action {
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
                OperationOverlay::RewindingThread,
            ),
            HistoryAction::Restore { checkpoint_id } => (
                HistoryOperation::Restore { checkpoint_id },
                None,
                OperationOverlay::RestoringCheckpoint,
            ),
        };
        self.overlay = Overlay::Operation(progress);
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
        if self.checkpoint().is_some() {
            bail!("cannot continue a checkpoint view");
        }
        if self.turn_is_running() {
            bail!("a turn is already running");
        }
        self.turn = TurnState::Starting {
            phase: TurnPhase::Running,
        };
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
        if self.checkpoint().is_some() {
            bail!("cannot compact a checkpoint view");
        }
        if self.turn_is_running() {
            bail!("a turn is already running");
        }
        self.turn = TurnState::Starting {
            phase: TurnPhase::Compacting,
        };
        effects
            .send(Effect::CompactTurn {
                endpoint: self.endpoint.clone(),
                thread_id,
            })
            .ok();
        Ok(())
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if self.error.is_some()
            || matches!(
                self.overlay,
                Overlay::Help
                    | Overlay::ThreadPicker(_)
                    | Overlay::LoadingCheckpoints
                    | Overlay::NoCheckpoints
                    | Overlay::Operation(_)
                    | Overlay::HistoryConfirmation(_)
            )
        {
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

        if matches!(self.overlay, Overlay::Command) {
            let area = self.layout.command_input_area;
            let scroll = self.layout.command_input_scroll;
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if area.contains((mouse.column, mouse.row).into()) =>
                {
                    let offset = input_offset_at(
                        &self.command_input.value,
                        area,
                        (0, scroll),
                        mouse.column,
                        mouse.row,
                    );
                    self.command_input.begin_selection(offset);
                }
                MouseEventKind::Drag(MouseButton::Left) if self.command_input.is_selecting() => {
                    let offset = input_offset_at(
                        &self.command_input.value,
                        area,
                        (0, scroll),
                        mouse.column,
                        mouse.row,
                    );
                    self.command_input.extend_selection(offset);
                }
                _ => {}
            }
            return Ok(());
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
        if self.target.checkpoint_picker().is_none() {
            let area = self.layout.input_text_area;
            let scroll = self.layout.input_scroll;
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if self
                        .layout
                        .input_area
                        .contains((mouse.column, mouse.row).into())
                        && self.composer_input().is_some() =>
                {
                    let offset = input_offset_at(
                        &self.composer_input().unwrap().value,
                        area,
                        scroll,
                        mouse.column,
                        mouse.row,
                    );
                    self.clear_selection();
                    self.view.focus = FocusPane::Input;
                    self.composer_input_mut().unwrap().begin_selection(offset);
                    return Ok(());
                }
                MouseEventKind::Drag(MouseButton::Left)
                    if self.view.focus == FocusPane::Input
                        && self.composer_input().is_some_and(InputBuffer::is_selecting) =>
                {
                    let offset = input_offset_at(
                        &self.composer_input().unwrap().value,
                        area,
                        scroll,
                        mouse.column,
                        mouse.row,
                    );
                    self.composer_input_mut().unwrap().extend_selection(offset);
                    return Ok(());
                }
                _ => {}
            }
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
        self.turn = TurnState::Starting {
            phase: TurnPhase::Running,
        };
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
        let started = self.active_turn().is_some();
        self.turn = TurnState::Cancelling;
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
        self.overlay = Overlay::Operation(OperationOverlay::RenamingThread);
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
        let selected = picker.selected_model().context("no model is selected")?;
        let provider = selected.provider.clone();
        let model = selected.id.clone();
        let reasoning_effort = selected
            .supported_reasoning_efforts
            .get(picker.effort_index)
            .cloned()
            .unwrap_or_else(|| selected.default_reasoning_effort.clone());
        let Some(thread_id) = self.target.thread_id() else {
            self.target = Target::New {
                model: Some((provider, model, reasoning_effort)),
            };
            self.overlay = Overlay::None;
            return Ok(());
        };
        self.overlay = Overlay::Operation(OperationOverlay::ChangingModel);
        effects
            .send(Effect::ChangeModel {
                endpoint: self.endpoint.clone(),
                thread_id,
                provider,
                model,
                reasoning_effort,
            })
            .ok();
        Ok(())
    }

    fn resolve_approval(
        &mut self,
        approval_id: InteractionId,
        allowed: bool,
        reason: Option<String>,
        effects: &mpsc::UnboundedSender<Effect>,
    ) {
        let decision = if allowed {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny { reason }
        };
        if self
            .pending_approval()
            .is_none_or(|approval| approval.id() != approval_id)
        {
            return;
        }
        self.turn = TurnState::ResolvingApproval { approval_id };
        self.overlay = Overlay::None;
        effects
            .send(Effect::ResolveApproval {
                endpoint: self.endpoint.clone(),
                approval_id,
                decision,
            })
            .ok();
    }

    pub(super) fn restore_failed_approval(&mut self, approval_id: InteractionId) {
        if matches!(
            self.turn,
            TurnState::ResolvingApproval {
                approval_id: current,
                ..
            } if current == approval_id
        ) {
            self.sync_turn_interaction();
        }
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

    fn composer_input(&self) -> Option<&InputBuffer> {
        match &self.turn {
            TurnState::EnteringDenyReason { reason, .. } => Some(reason),
            TurnState::AnsweringQuestions(form) if form.mode == QuestionFormMode::Note => {
                Some(&form.drafts[form.current].note)
            }
            _ => matches!(self.overlay, Overlay::None | Overlay::Rename)
                .then_some(&self.message_input),
        }
    }

    fn composer_input_mut(&mut self) -> Option<&mut InputBuffer> {
        match &mut self.turn {
            TurnState::EnteringDenyReason { reason, .. } => Some(reason),
            TurnState::AnsweringQuestions(form) if form.mode == QuestionFormMode::Note => {
                Some(&mut form.drafts[form.current].note)
            }
            _ => matches!(self.overlay, Overlay::None | Overlay::Rename)
                .then_some(&mut self.message_input),
        }
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
        self.copy_text(&text[start..end])
    }

    fn copy_text(&mut self, text: &str) -> Result<()> {
        write!(
            io::stdout(),
            "\x1b]52;c;{}\x07",
            STANDARD.encode(text.as_bytes())
        )
        .context("failed to write OSC 52 clipboard sequence")?;
        io::stdout().flush().context("failed to flush OSC 52")?;
        Ok(())
    }

    fn handle_ctrl_c(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        if let Some(text) = self.input_selection_text() {
            return self.copy_text(&text);
        }
        if self
            .selection_range()
            .is_some_and(|(start, end)| start != end)
        {
            return self.copy_selection();
        }
        if self.turn_is_running() {
            self.cancel_turn(effects);
            return Ok(());
        }

        if matches!(self.overlay, Overlay::Command) {
            self.command_input.clear();
        } else if let TurnState::EnteringDenyReason { reason, .. } = &mut self.turn {
            reason.clear();
        } else if !self.message_input.value.is_empty() {
            let input = self.message_input.take();
            history::record(&self.message_history_path, &mut self.message_input, input)?;
        }
        Ok(())
    }

    fn input_selection_text(&self) -> Option<String> {
        let input = if matches!(self.overlay, Overlay::Command) {
            &self.command_input
        } else if let TurnState::EnteringDenyReason { reason, .. } = &self.turn {
            reason
        } else {
            &self.message_input
        };
        let (start, end) = input.selection_range()?;
        Some(input.value[start..end].to_owned())
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
