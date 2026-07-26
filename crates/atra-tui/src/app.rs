use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use atra_protocol::{
    ControllerRequest, ControllerResponse, Model, Thread, ThreadCheckpoint, ThreadEvent,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use icu_segmenter::{WordSegmenter, WordSegmenterBorrowed, options::WordBreakInvariantOptions};
use tokio::sync::mpsc;

use crate::{
    controller::request,
    history,
    input::{InputAction, InputBuffer},
    layout::{SelectionPoint, ViewLayout},
    runtime::Effect,
    state::{
        Approval, CheckpointPicker, FocusPane, HistoryAction, ModelPicker, Overlay, ThreadPicker,
        TranscriptMode, TurnState, ViewState,
    },
    transcript::{
        Author, TranscriptEntry, TranscriptItem, item_from_event, sanitize, transcript_text,
    },
};

pub(crate) const COMMAND_HELP: &[(&str, &str)] = &[
    ("/view", "Switch between coding and debug views"),
    ("/thread", "Select a thread"),
    ("/new", "Start a new thread"),
    ("/model", "Select the model and reasoning effort"),
    ("/checkpoint", "Save the current thread"),
    ("/checkpoints", "Browse saved checkpoints"),
    ("/fork", "Fork at the selected message"),
    ("/rewind", "Rewind to the selected message"),
    ("/restore", "Restore the displayed checkpoint"),
    ("/continue", "Continue an incomplete turn"),
    ("/help", "Show this command list"),
    ("/exit", "Exit Atra"),
];

pub(crate) struct TurnCompletion {
    pub(super) thread_id: i64,
    pub(super) response: ControllerResponse,
}

pub(crate) enum Activity {
    Info(String),
    Error(String),
}

pub(crate) enum TurnUpdate {
    Started {
        message: String,
        thread_id: i64,
        threads: Vec<Thread>,
    },
    Delta {
        thread_id: i64,
        content: String,
    },
    ReasoningSummaryDelta {
        thread_id: i64,
        content: String,
    },
    ReasoningSummaryPartAdded {
        thread_id: i64,
    },
    ToolCallStarted {
        thread_id: i64,
        item_id: String,
        name: String,
    },
    Event {
        thread_id: i64,
        event: ThreadEvent,
    },
    Completed(Result<TurnCompletion>),
    LoginCompleted(Result<ControllerResponse>),
    ThreadSelected {
        thread_id: i64,
        result: Result<(Vec<TranscriptEntry>, Vec<ThreadEvent>)>,
    },
    ThreadRenamed {
        thread_id: i64,
        display_name: String,
        result: Result<ControllerResponse>,
    },
    ModelChanged {
        thread_id: i64,
        model: String,
        reasoning_effort: String,
        result: Result<ControllerResponse>,
    },
    CheckpointsLoaded {
        thread_id: i64,
        result: Result<Vec<ThreadCheckpoint>>,
    },
    CheckpointLoaded(Result<(ThreadCheckpoint, Vec<ThreadEvent>)>),
    HistoryChanged {
        source_thread_id: i64,
        draft: Option<String>,
        result: Result<(
            ControllerResponse,
            i64,
            Vec<Thread>,
            (Vec<TranscriptEntry>, Vec<ThreadEvent>),
        )>,
    },
}

pub(crate) struct App {
    pub(crate) endpoint: PathBuf,
    pub(crate) message_history_path: PathBuf,
    pub(crate) command_history_path: PathBuf,
    pub(crate) threads: Vec<Thread>,
    pub(crate) models: Vec<Model>,
    pub(crate) thread_id: Option<i64>,
    pub(crate) transcript: Vec<TranscriptEntry>,
    pub(crate) events: Vec<ThreadEvent>,
    pub(crate) checkpoint: Option<ThreadCheckpoint>,
    pub(crate) tool_call_previews: HashMap<String, usize>,
    pub(crate) message_input: InputBuffer,
    pub(crate) command_input: InputBuffer,
    pub(crate) overlay: Overlay,
    pub(crate) word_segmenter: WordSegmenterBorrowed<'static>,
    pub(crate) activity: Option<Activity>,
    pub(crate) new_thread_model: Option<(String, String)>,
    pub(crate) login_required: bool,
    pub(crate) view: ViewState,
    pub(crate) layout: ViewLayout,
    pub(crate) turn: TurnState,
    pub(crate) metrics_stale: bool,
}

impl App {
    pub(super) async fn load(
        endpoint: PathBuf,
        message_history_path: PathBuf,
        command_history_path: PathBuf,
    ) -> Result<Self> {
        let threads = match request(&endpoint, ControllerRequest::ThreadList).await? {
            ControllerResponse::ThreadList { threads } => threads,
            ControllerResponse::Error { message } => bail!("{message}"),
            response => bail!("controller returned an unexpected response: {response:?}"),
        };
        let thread_id = threads.first().map(|thread| thread.id);
        let (transcript, events) = match thread_id {
            Some(thread_id) => load_transcript(&endpoint, thread_id).await?,
            None => (Vec::new(), Vec::new()),
        };
        let login_required = match request(&endpoint, ControllerRequest::CodexLoginStatus).await? {
            ControllerResponse::CodexLoginRequired => true,
            ControllerResponse::CodexLoggedIn { .. } => false,
            ControllerResponse::Error { message } => bail!("{message}"),
            response => bail!("controller returned an unexpected response: {response:?}"),
        };
        let models = match request(&endpoint, ControllerRequest::ModelList).await? {
            ControllerResponse::ModelList { models } => models,
            ControllerResponse::Error { .. } => Vec::new(),
            response => bail!("controller returned an unexpected response: {response:?}"),
        };
        let message_history = history::load(&message_history_path)?;
        let command_history = history::load(&command_history_path)?;
        Ok(Self {
            endpoint,
            message_history_path,
            command_history_path,
            threads,
            models,
            thread_id,
            transcript,
            events,
            checkpoint: None,
            tool_call_previews: HashMap::new(),
            message_input: InputBuffer::new(message_history, true),
            command_input: InputBuffer::new(command_history, false),
            overlay: Overlay::None,
            word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
            activity: Some(Activity::Info(if login_required {
                "Codex login required · Ctrl-L login".to_owned()
            } else {
                "/thread · /new · /model · Ctrl-P/Ctrl-/ command · Tab focus · Ctrl-C copies"
                    .to_owned()
            })),
            new_thread_model: None,
            login_required,
            view: ViewState::default(),
            layout: ViewLayout::default(),
            turn: TurnState::Idle,
            metrics_stale: false,
        })
    }

    pub(super) fn handle_key(
        &mut self,
        key: KeyEvent,
        effects: &mpsc::UnboundedSender<Effect>,
    ) -> Result<bool> {
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

        if let Overlay::Approval(approval) = &mut self.overlay {
            if let Some(reason) = &mut approval.deny_reason {
                match key.code {
                    KeyCode::Enter => {
                        let id = approval.id;
                        let reason = reason.take();
                        let reason = (!reason.trim().is_empty()).then_some(reason);
                        self.resolve_approval(id, false, reason, effects);
                    }
                    KeyCode::Esc => approval.deny_reason = None,
                    _ => {
                        reason.handle_key(key, &self.word_segmenter);
                    }
                }
            } else {
                match key.code {
                    KeyCode::Char('y') => {
                        let id = approval.id;
                        self.resolve_approval(id, true, None, effects);
                    }
                    KeyCode::Char('n') => {
                        approval.deny_reason = Some(InputBuffer::new(Vec::new(), false));
                    }
                    _ => {}
                }
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
                KeyCode::Char('r') if self.thread_id.is_some() => {
                    self.overlay = Overlay::Rename;
                    self.view.focus = FocusPane::Input;
                    let display_name = self
                        .threads
                        .iter()
                        .find(|thread| Some(thread.id) == self.thread_id)
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
                    if picker.selecting_effort {
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
                    if picker.selecting_effort {
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
                KeyCode::Enter if picker.selecting_effort => self.change_model(effects)?,
                KeyCode::Enter => {
                    picker.selecting_effort = true;
                    self.activity = Some(Activity::Info(
                        "Select reasoning effort · Enter applies · Esc goes back".to_owned(),
                    ));
                }
                KeyCode::Esc => {
                    if picker.selecting_effort {
                        picker.selecting_effort = false;
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

        if let Overlay::CheckpointPicker(picker) = &mut self.overlay {
            match key.code {
                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                KeyCode::Down => {
                    picker.selected =
                        (picker.selected + 1).min(picker.checkpoints.len().saturating_sub(1));
                }
                KeyCode::Enter if !picker.checkpoints.is_empty() => {
                    let checkpoint = picker.checkpoints[picker.selected].clone();
                    self.overlay = Overlay::None;
                    self.activity = Some(Activity::Info("Loading checkpoint…".to_owned()));
                    effects
                        .send(Effect::LoadCheckpoint {
                            endpoint: self.endpoint.clone(),
                            checkpoint,
                        })
                        .ok();
                }
                KeyCode::Esc => {
                    self.overlay = Overlay::None;
                    self.activity = None;
                }
                _ => {}
            }
            return Ok(false);
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
            && let (Some(thread_id), Some(_)) = (self.thread_id, self.checkpoint.take())
        {
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
            "view" => {
                self.view.transcript_mode = match self.view.transcript_mode {
                    TranscriptMode::Coding => TranscriptMode::Debug,
                    TranscriptMode::Debug => TranscriptMode::Coding,
                };
                self.view.focus = FocusPane::Input;
                self.clear_selection();
                self.activity = Some(Activity::Info(
                    match self.view.transcript_mode {
                        TranscriptMode::Coding => "Coding transcript",
                        TranscriptMode::Debug => "LLM request inspector",
                    }
                    .to_owned(),
                ));
            }
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

    fn start_new_thread(&mut self) {
        self.thread_id = None;
        self.transcript.clear();
        self.events.clear();
        self.checkpoint = None;
        self.tool_call_previews.clear();
        self.message_input.clear();
        self.overlay = Overlay::None;
        self.new_thread_model = None;
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
            .find(|thread| Some(thread.id) == self.thread_id)
            .map(|thread| (thread.model.as_str(), thread.reasoning_effort.as_str()))
            .or_else(|| {
                self.new_thread_model
                    .as_ref()
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
            selecting_effort: false,
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
            .position(|thread| Some(thread.id) == self.thread_id)
            .unwrap_or(0);
        self.overlay = Overlay::ThreadPicker(ThreadPicker { selected });
        self.activity = Some(Activity::Info(
            "Select thread · Enter switches · Esc cancels".to_owned(),
        ));
    }

    fn select_thread(&mut self, thread_id: i64, effects: &mpsc::UnboundedSender<Effect>) {
        self.activity = Some(Activity::Info("Loading thread…".to_owned()));
        effects
            .send(Effect::SelectThread {
                endpoint: self.endpoint.clone(),
                thread_id,
            })
            .ok();
    }

    fn create_checkpoint(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.thread_id.context("no thread is selected")?;
        if self.checkpoint.is_some() {
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
                request: ControllerRequest::ThreadCheckpointCreate { thread_id },
            })
            .ok();
        Ok(())
    }

    fn open_checkpoints(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.thread_id.context("no thread is selected")?;
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

    fn selected_history_point(&self) -> Result<(i64, Option<String>)> {
        let index = self
            .view
            .selected_item
            .context("select a user or assistant message in the transcript first")?;
        let entry = &self.transcript[index];
        if !entry.is_assistant_message() && entry.user_message().is_none() {
            bail!("the selected transcript item is not a user or assistant message");
        }
        let sequence = entry
            .sequence
            .context("the selected transcript item has not been saved yet")?;
        Ok((sequence, entry.user_message().map(str::to_owned)))
    }

    fn fork_selected(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.thread_id.context("no thread is selected")?;
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
                request: ControllerRequest::ThreadFork {
                    thread_id,
                    checkpoint_id: self.checkpoint.as_ref().map(|checkpoint| checkpoint.id),
                    sequence,
                    display_name: None,
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
            checkpoint_id: self.checkpoint.as_ref().map(|checkpoint| checkpoint.id),
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
            .checkpoint
            .as_ref()
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
        let thread_id = self.thread_id.context("no thread is selected")?;
        let (request, draft) = match action {
            HistoryAction::Rewind {
                checkpoint_id,
                sequence,
                draft,
            } => (
                ControllerRequest::ThreadRewind {
                    thread_id,
                    checkpoint_id,
                    sequence,
                },
                draft,
            ),
            HistoryAction::Restore { checkpoint_id } => (
                ControllerRequest::ThreadCheckpointRestore {
                    thread_id,
                    checkpoint_id,
                },
                None,
            ),
        };
        self.activity = Some(Activity::Info("Updating thread history…".to_owned()));
        effects
            .send(Effect::HistoryRequest {
                endpoint: self.endpoint.clone(),
                thread_id,
                draft,
                request,
            })
            .ok();
        Ok(())
    }

    fn continue_thread(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.thread_id.context("no thread is selected")?;
        if self.checkpoint.is_some() {
            bail!("cannot continue a checkpoint view");
        }
        if self.turn.is_running() {
            bail!("a turn is already running");
        }
        self.turn = TurnState::Running;
        self.activity = Some(Activity::Info("Continuing turn…".to_owned()));
        effects
            .send(Effect::ResumeTurn {
                endpoint: self.endpoint.clone(),
                thread_id,
                request: ControllerRequest::ThreadContinue { thread_id },
            })
            .ok();
        Ok(())
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if matches!(
            self.overlay,
            Overlay::Help
                | Overlay::ThreadPicker(_)
                | Overlay::CheckpointPicker(_)
                | Overlay::HistoryConfirmation(_)
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
                if self
                    .layout
                    .detail_area
                    .contains((mouse.column, mouse.row).into())
                {
                    self.view.detail_scroll = self.view.detail_scroll.saturating_sub(3);
                    self.view.focus = FocusPane::Detail;
                } else {
                    self.view.transcript_scroll = self.view.transcript_scroll.saturating_add(3);
                    self.view.focus = FocusPane::Transcript;
                }
                return Ok(());
            }
            MouseEventKind::ScrollDown => {
                if self
                    .layout
                    .detail_area
                    .contains((mouse.column, mouse.row).into())
                {
                    self.view.detail_scroll = self.view.detail_scroll.saturating_add(3);
                    self.view.focus = FocusPane::Detail;
                } else {
                    self.view.transcript_scroll = self.view.transcript_scroll.saturating_sub(3);
                    self.view.focus = FocusPane::Transcript;
                }
                return Ok(());
            }
            _ => {}
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self
                .layout
                .input_area
                .contains((mouse.column, mouse.row).into())
        {
            self.view.focus = FocusPane::Input;
            return Ok(());
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self
                .layout
                .request_list_area
                .contains((mouse.column, mouse.row).into())
        {
            let row = usize::from(
                mouse
                    .row
                    .saturating_sub(self.layout.request_list_area.y + 1),
            ) / 3;
            if row < self.request_count() {
                self.view.selected_request = Some(row);
                self.view.detail_scroll = 0;
            }
            self.view.focus = FocusPane::Requests;
            return Ok(());
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self
                .layout
                .detail_area
                .contains((mouse.column, mouse.row).into())
        {
            self.view.focus = FocusPane::Detail;
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
                && self.transcript[index].is_tool_result()
                && !self.view.expanded_tools.remove(&index)
            {
                self.view.expanded_tools.insert(index);
            }
            self.view.selected_item = Some(index);
            self.view.focus = FocusPane::Transcript;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.view.transcript_mode == TranscriptMode::Coding
                    && self
                        .layout
                        .transcript_area
                        .contains((mouse.column, mouse.row).into())
                {
                    self.view.focus = FocusPane::Transcript;
                }
                self.view.selection_start = self.point_at(mouse.column, mouse.row);
                self.view.selection_end = self.view.selection_start;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.view.selection_start.is_some() {
                    self.view.selection_end = self.point_at(mouse.column, mouse.row);
                }
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
        self.turn = TurnState::Running;
        self.activity = Some(Activity::Info("Waiting for Atra Controller…".to_owned()));
        effects
            .send(Effect::SendTurn {
                endpoint: self.endpoint.clone(),
                thread_id: self.thread_id,
                new_thread_model: self.new_thread_model.take(),
                message,
            })
            .ok();
        Ok(())
    }

    fn rename(&mut self, effects: &mpsc::UnboundedSender<Effect>) -> Result<()> {
        let thread_id = self.thread_id.context("no thread is selected")?;
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
        let Some(thread_id) = self.thread_id else {
            self.new_thread_model = Some((model, reasoning_effort));
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
        approval_id: u64,
        allowed: bool,
        reason: Option<String>,
        effects: &mpsc::UnboundedSender<Effect>,
    ) {
        let request_message = if allowed {
            ControllerRequest::ApprovalAllow { approval_id }
        } else {
            ControllerRequest::ApprovalDeny {
                approval_id,
                reason,
            }
        };
        self.overlay = Overlay::None;
        self.turn = TurnState::Running;
        self.activity = Some(Activity::Info("Waiting for Atra Controller…".to_owned()));
        let thread_id = self.thread_id.expect("approval belongs to a thread");
        effects
            .send(Effect::ResumeTurn {
                endpoint: self.endpoint.clone(),
                thread_id,
                request: request_message,
            })
            .ok();
    }

    pub(super) fn update(&mut self, update: TurnUpdate) -> Result<()> {
        let completion = match update {
            TurnUpdate::Started {
                message,
                thread_id,
                threads,
            } => {
                self.threads = threads;
                if self.thread_id.is_none() {
                    self.thread_id = Some(thread_id);
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
            TurnUpdate::Delta { thread_id, content } => {
                if self.thread_id == Some(thread_id) {
                    if self
                        .transcript
                        .last()
                        .is_some_and(TranscriptEntry::is_assistant_message)
                    {
                        self.transcript
                            .last_mut()
                            .unwrap()
                            .append_message(&sanitize(&content));
                    } else {
                        self.transcript.push(TranscriptEntry::message(
                            Author::Assistant,
                            sanitize(&content),
                        ));
                    }
                }
                return Ok(());
            }
            TurnUpdate::ReasoningSummaryDelta { thread_id, content } => {
                if self.thread_id == Some(thread_id) {
                    let content = sanitize(&content);
                    if self
                        .transcript
                        .last()
                        .is_some_and(TranscriptEntry::is_reasoning_summary)
                    {
                        self.transcript.last_mut().unwrap().append_message(&content);
                    } else {
                        self.transcript.push(TranscriptEntry::new(
                            TranscriptItem::ReasoningSummary { text: content },
                        ));
                    }
                }
                return Ok(());
            }
            TurnUpdate::ReasoningSummaryPartAdded { thread_id } => {
                if self.thread_id == Some(thread_id)
                    && let Some(entry) = self.transcript.last_mut()
                    && entry.is_reasoning_summary()
                    && !entry.is_empty_reasoning_summary()
                {
                    entry.append_message("\n\n");
                }
                return Ok(());
            }
            TurnUpdate::ToolCallStarted {
                thread_id,
                item_id,
                name,
            } => {
                if self.thread_id == Some(thread_id) {
                    let index = self.transcript.len();
                    self.transcript
                        .push(TranscriptEntry::new(TranscriptItem::ToolCall {
                            name: sanitize(&name),
                            arguments: None,
                        }));
                    self.tool_call_previews.insert(item_id, index);
                }
                return Ok(());
            }
            TurnUpdate::Event { thread_id, event } => {
                if self.thread_id == Some(thread_id) {
                    let usage_matches_selected_model = event.kind == "token_usage"
                        && event.payload["request_sequence"]
                            .as_i64()
                            .and_then(|sequence| {
                                self.events.iter().find(|event| event.sequence == sequence)
                            })
                            .and_then(|event| event.payload.pointer("/request/model"))
                            .and_then(serde_json::Value::as_str)
                            .zip(
                                self.threads
                                    .iter()
                                    .find(|thread| thread.id == thread_id)
                                    .map(|thread| thread.model.as_str()),
                            )
                            .is_some_and(|(request_model, selected_model)| {
                                request_model == selected_model
                            });
                    if usage_matches_selected_model {
                        self.metrics_stale = false;
                    }
                    self.events.push(event.clone());
                    let item_id = event
                        .payload
                        .get("item_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    let sequence = event.sequence;
                    let Some(item) = item_from_event(event) else {
                        return Ok(());
                    };
                    if matches!(item, TranscriptItem::ToolCall { .. })
                        && let Some(item_id) = item_id
                        && let Some(index) = self.tool_call_previews.remove(&item_id)
                    {
                        self.transcript[index].replace_event(sequence, item);
                        return Ok(());
                    }
                    if matches!(
                        item,
                        TranscriptItem::Message {
                            author: Author::Assistant,
                            ..
                        }
                    ) && let Some(entry) = self.transcript.last_mut()
                        && entry.is_assistant_message()
                        && entry.sequence.is_none()
                    {
                        entry.replace_event(sequence, item);
                        return Ok(());
                    }
                    self.transcript.push(TranscriptEntry {
                        item,
                        sequence: Some(sequence),
                        rendered: None,
                    });
                }
                return Ok(());
            }
            TurnUpdate::Completed(Ok(completion)) => completion,
            TurnUpdate::Completed(Err(error)) => {
                let mut preview_indices = self
                    .tool_call_previews
                    .drain()
                    .map(|(_, index)| index)
                    .collect::<Vec<_>>();
                preview_indices.sort_unstable_by(|left, right| right.cmp(left));
                for index in preview_indices {
                    self.transcript.remove(index);
                }
                self.turn = TurnState::Idle;
                self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                return Ok(());
            }
            TurnUpdate::LoginCompleted(result) => {
                match result? {
                    ControllerResponse::CodexLoggedIn { .. } => {
                        self.login_required = false;
                        self.activity = Some(Activity::Info("Codex login complete".to_owned()));
                    }
                    ControllerResponse::Error { message } => {
                        self.activity = Some(Activity::Error(sanitize(&message)));
                    }
                    response => bail!("controller returned an unexpected response: {response:?}"),
                }
                return Ok(());
            }
            TurnUpdate::ThreadSelected { thread_id, result } => {
                let (transcript, events) = result?;
                self.thread_id = Some(thread_id);
                self.transcript = transcript;
                self.events = events;
                self.checkpoint = None;
                self.tool_call_previews.clear();
                self.overlay = Overlay::None;
                self.clear_selection();
                self.reset_view();
                self.metrics_stale = false;
                self.activity = Some(Activity::Info("Thread selected".to_owned()));
                return Ok(());
            }
            TurnUpdate::ThreadRenamed {
                thread_id,
                display_name,
                result,
            } => {
                match result? {
                    ControllerResponse::ThreadRenamed => {
                        if let Some(thread) = self
                            .threads
                            .iter_mut()
                            .find(|thread| thread.id == thread_id)
                        {
                            thread.display_name = Some(display_name);
                        }
                        self.activity = Some(Activity::Info("Thread renamed".to_owned()));
                    }
                    ControllerResponse::Error { message } => {
                        self.activity = Some(Activity::Error(sanitize(&message)));
                    }
                    response => bail!("controller returned an unexpected response: {response:?}"),
                }
                return Ok(());
            }
            TurnUpdate::ModelChanged {
                thread_id,
                model,
                reasoning_effort,
                result,
            } => {
                match result? {
                    ControllerResponse::ThreadModelChanged => {
                        if let Some(thread) = self
                            .threads
                            .iter_mut()
                            .find(|thread| thread.id == thread_id)
                        {
                            thread.model = model;
                            thread.reasoning_effort = reasoning_effort;
                        }
                        self.metrics_stale = true;
                        self.activity = Some(Activity::Info("Thread model changed".to_owned()));
                    }
                    ControllerResponse::Error { message } => {
                        self.activity = Some(Activity::Error(sanitize(&message)));
                    }
                    response => bail!("controller returned an unexpected response: {response:?}"),
                }
                return Ok(());
            }
            TurnUpdate::CheckpointsLoaded { thread_id, result } => {
                if self.thread_id != Some(thread_id) {
                    return Ok(());
                }
                let checkpoints = result?;
                if checkpoints.is_empty() {
                    self.activity = Some(Activity::Info("No checkpoints are available".to_owned()));
                } else {
                    self.overlay = Overlay::CheckpointPicker(CheckpointPicker {
                        checkpoints,
                        selected: 0,
                    });
                    self.activity = Some(Activity::Info(
                        "Select checkpoint · Enter opens · Esc cancels".to_owned(),
                    ));
                }
                return Ok(());
            }
            TurnUpdate::CheckpointLoaded(result) => {
                let (checkpoint, events) = result?;
                if self.thread_id != Some(checkpoint.thread_id) {
                    return Ok(());
                }
                self.transcript = events
                    .iter()
                    .cloned()
                    .filter_map(TranscriptEntry::from_event)
                    .collect();
                self.events = events;
                self.checkpoint = Some(checkpoint);
                self.overlay = Overlay::None;
                self.clear_selection();
                self.reset_view();
                self.activity = Some(Activity::Info(
                    "Checkpoint view · Esc returns to active history".to_owned(),
                ));
                return Ok(());
            }
            TurnUpdate::HistoryChanged {
                source_thread_id,
                draft,
                result,
            } => {
                if self.thread_id != Some(source_thread_id) {
                    return Ok(());
                }
                let (response, thread_id, threads, (transcript, events)) = result?;
                let message = match response {
                    ControllerResponse::ThreadCheckpointCreated { checkpoint_id } => {
                        format!("Checkpoint {checkpoint_id} created")
                    }
                    ControllerResponse::ThreadCheckpointRestored => {
                        "Checkpoint restored".to_owned()
                    }
                    ControllerResponse::ThreadForked { .. } => "Thread forked".to_owned(),
                    ControllerResponse::ThreadRewound => "Thread rewound".to_owned(),
                    response => {
                        bail!("controller returned an unexpected history response: {response:?}")
                    }
                };
                self.thread_id = Some(thread_id);
                self.threads = threads;
                self.transcript = transcript;
                self.events = events;
                self.checkpoint = None;
                self.overlay = Overlay::None;
                self.tool_call_previews.clear();
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
        };
        self.turn = TurnState::Idle;
        if self.thread_id == Some(completion.thread_id) {
            match completion.response {
                ControllerResponse::TurnCompleted { .. }
                    if self
                        .transcript
                        .last()
                        .is_some_and(TranscriptEntry::is_assistant_message) =>
                {
                    self.activity = None;
                }
                response => self.accept_turn_response(response)?,
            }
        } else {
            self.activity = None;
        }
        Ok(())
    }

    fn accept_turn_response(&mut self, response: ControllerResponse) -> Result<()> {
        match response {
            ControllerResponse::TurnCompleted { content } => {
                self.transcript.push(TranscriptEntry::message(
                    Author::Assistant,
                    sanitize(&content),
                ));
                self.activity = None;
            }
            ControllerResponse::ApprovalRequired { approval_id, .. } => {
                self.overlay = Overlay::Approval(Approval {
                    id: approval_id,
                    deny_reason: None,
                });
                self.activity = None;
            }
            ControllerResponse::Error { message } => {
                self.activity = Some(Activity::Error(sanitize(&message)));
            }
            response => bail!("controller returned an unexpected response: {response:?}"),
        }
        Ok(())
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
        let text = transcript_text(&self.transcript);
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

    fn clear_selection(&mut self) {
        self.view.selection_start = None;
        self.view.selection_end = None;
    }

    fn reset_view(&mut self) {
        self.view.transcript_scroll = 0;
        self.view.detail_scroll = 0;
        self.view.selected_request = None;
        self.view.raw_request = false;
        self.view.expanded_tools.clear();
        self.view.selected_item = None;
        self.view.focus = FocusPane::Input;
    }

    fn cycle_focus(&mut self, reverse: bool) {
        let panes: &[FocusPane] = match self.view.transcript_mode {
            TranscriptMode::Coding => &[FocusPane::Input, FocusPane::Transcript],
            TranscriptMode::Debug => &[FocusPane::Input, FocusPane::Requests, FocusPane::Detail],
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
                        && self.transcript[index].is_tool_result()
                        && !self.view.expanded_tools.remove(&index)
                    {
                        self.view.expanded_tools.insert(index);
                    }
                }
                KeyCode::End => self.view.transcript_scroll = 0,
                _ => {}
            },
            FocusPane::Requests => match key.code {
                KeyCode::Up => {
                    let selected = self.view.selected_request.unwrap_or(self.request_count());
                    self.view.selected_request = Some(selected.saturating_sub(1));
                    self.view.detail_scroll = 0;
                }
                KeyCode::Down => {
                    let last = self.request_count().saturating_sub(1);
                    self.view.selected_request = Some(
                        self.view
                            .selected_request
                            .unwrap_or(last)
                            .saturating_add(1)
                            .min(last),
                    );
                    self.view.detail_scroll = 0;
                }
                _ => {}
            },
            FocusPane::Detail => match key.code {
                KeyCode::Up => self.view.detail_scroll = self.view.detail_scroll.saturating_sub(1),
                KeyCode::Down => {
                    self.view.detail_scroll = self.view.detail_scroll.saturating_add(1)
                }
                KeyCode::PageUp => {
                    self.view.detail_scroll = self
                        .view
                        .detail_scroll
                        .saturating_sub(usize::from(self.layout.detail_area.height))
                }
                KeyCode::PageDown => {
                    self.view.detail_scroll = self
                        .view
                        .detail_scroll
                        .saturating_add(usize::from(self.layout.detail_area.height))
                }
                KeyCode::Char('r') => {
                    self.view.raw_request = !self.view.raw_request;
                    self.view.detail_scroll = 0;
                }
                KeyCode::End => self.view.detail_scroll = 0,
                _ => {}
            },
            FocusPane::Input => {}
        }
    }

    fn request_count(&self) -> usize {
        self.events
            .iter()
            .filter(|event| event.kind == "model_request")
            .count()
    }

    fn select_item(&mut self, forward: bool) {
        if self.transcript.is_empty() {
            self.view.selected_item = None;
            return;
        }
        let next = match (self.view.selected_item, forward) {
            (Some(index), true) => (index + 1).min(self.transcript.len() - 1),
            (Some(index), false) => index.saturating_sub(1),
            (None, true) => 0,
            (None, false) => self.transcript.len() - 1,
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

pub(super) async fn load_transcript(
    endpoint: &Path,
    thread_id: i64,
) -> Result<(Vec<TranscriptEntry>, Vec<ThreadEvent>)> {
    match request(endpoint, ControllerRequest::ThreadEvents { thread_id }).await? {
        ControllerResponse::ThreadEvents { events } => {
            let mut transcript = Vec::new();
            for event in events.iter().cloned() {
                if let Some(item) = TranscriptEntry::from_event(event) {
                    transcript.push(item);
                }
            }
            Ok((transcript, events))
        }
        ControllerResponse::Error { message } => bail!("{message}"),
        response => bail!("controller returned an unexpected response: {response:?}"),
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
