use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_protocol::{ControllerRequest, ControllerResponse, Model, Thread, ThreadEvent};
use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures_util::StreamExt;
use icu_segmenter::{WordSegmenter, WordSegmenterBorrowed, options::WordBreakInvariantOptions};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use tokio::sync::mpsc;
use tui_markdown::{Options as MarkdownOptions, StyleSheet, from_str_with_options};
use unicode_width::UnicodeWidthChar;

use crate::{
    controller::{request, request_stream},
    input::{InputAction, InputBuffer},
};

pub(crate) const COMMAND_HELP: &[(&str, &str)] = &[
    ("/view", "Switch between coding and debug views"),
    ("/thread", "Select a thread"),
    ("/new", "Start a new thread"),
    ("/model", "Select the model and reasoning effort"),
    ("/help", "Show this command list"),
    ("/exit", "Exit Atra"),
];

#[derive(Clone)]
pub(crate) enum TranscriptItem {
    Message {
        author: Author,
        text: String,
    },
    ToolCall {
        name: String,
        arguments: Option<serde_json::Value>,
    },
    ToolResult {
        result: serde_json::Value,
    },
    Approval {
        id: u64,
        tool: Option<String>,
        allowed: Option<bool>,
    },
}

impl TranscriptItem {
    fn message(author: Author, text: String) -> Self {
        Self::Message { author, text }
    }

    fn append_message(&mut self, content: &str) {
        let Self::Message { text, .. } = self else {
            unreachable!()
        };
        text.push_str(content);
    }

    fn is_tool_result(&self) -> bool {
        matches!(self, Self::ToolResult { .. })
    }

    fn is_user_message(&self) -> bool {
        matches!(
            self,
            Self::Message {
                author: Author::User,
                ..
            }
        )
    }

    fn is_assistant_message(&self) -> bool {
        matches!(
            self,
            Self::Message {
                author: Author::Assistant,
                ..
            }
        )
    }
}

pub(crate) struct TranscriptEntry {
    item: TranscriptItem,
    rendered: Option<RenderedItem>,
}

impl TranscriptEntry {
    fn new(item: TranscriptItem) -> Self {
        Self {
            item,
            rendered: None,
        }
    }

    fn message(author: Author, text: String) -> Self {
        Self::new(TranscriptItem::message(author, text))
    }

    fn append_message(&mut self, content: &str) {
        self.item.append_message(content);
        self.rendered = None;
    }

    fn replace(&mut self, item: TranscriptItem) {
        self.item = item;
        self.rendered = None;
    }

    fn is_tool_result(&self) -> bool {
        self.item.is_tool_result()
    }

    fn is_assistant_message(&self) -> bool {
        self.item.is_assistant_message()
    }
}

struct RenderedItem {
    width: u16,
    expanded: bool,
    lines: Vec<DisplayedLine>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptMode {
    Coding,
    Debug,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusPane {
    Input,
    Transcript,
    Requests,
    Detail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Author {
    User,
    Assistant,
}

pub(crate) struct Approval {
    pub(crate) id: u64,
    pub(crate) deny_reason: Option<InputBuffer>,
}

pub(crate) struct ModelPicker {
    pub(crate) models: Vec<Model>,
    pub(crate) model_index: usize,
    pub(crate) effort_index: usize,
    pub(crate) selecting_effort: bool,
}

pub(crate) struct ThreadPicker {
    pub(crate) selected: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct SelectionPoint {
    offset: usize,
}

pub(crate) struct MappedRow {
    x: u16,
    y: u16,
    cells: Vec<usize>,
    end: usize,
}

pub(crate) struct TranscriptLayout {
    rows: Vec<MappedRow>,
}

#[derive(Clone)]
struct DisplayedLine {
    marker: Option<char>,
    line: Line<'static>,
    continuation: bool,
}

pub(crate) struct TurnCompletion {
    thread_id: i64,
    response: ControllerResponse,
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
    pub(crate) tool_call_preview: Option<(String, usize)>,
    pub(crate) message_input: InputBuffer,
    pub(crate) command_input: InputBuffer,
    pub(crate) command_open: bool,
    pub(crate) help_open: bool,
    pub(crate) word_segmenter: WordSegmenterBorrowed<'static>,
    pub(crate) activity: Option<Activity>,
    pub(crate) approval: Option<Approval>,
    pub(crate) renaming: bool,
    pub(crate) model_picker: Option<ModelPicker>,
    pub(crate) thread_picker: Option<ThreadPicker>,
    pub(crate) new_thread_model: Option<(String, String)>,
    pub(crate) login_required: bool,
    pub(crate) selection_start: Option<SelectionPoint>,
    pub(crate) selection_end: Option<SelectionPoint>,
    pub(crate) transcript_layout: TranscriptLayout,
    pub(crate) turn_pending: bool,
    pub(crate) metrics_stale: bool,
    pub(crate) transcript_mode: TranscriptMode,
    pub(crate) focus: FocusPane,
    pub(crate) transcript_scroll: usize,
    pub(crate) detail_scroll: usize,
    pub(crate) selected_request: Option<usize>,
    pub(crate) raw_request: bool,
    pub(crate) expanded_tools: HashSet<usize>,
    pub(crate) selected_item: Option<usize>,
    pub(crate) transcript_area: Rect,
    pub(crate) transcript_scrollbar_area: Rect,
    pub(crate) transcript_max_scroll: usize,
    pub(crate) transcript_scrollbar_thumb_start: u16,
    pub(crate) transcript_scrollbar_thumb_len: u16,
    pub(crate) transcript_scrollbar_drag_offset: Option<u16>,
    pub(crate) input_area: Rect,
    pub(crate) request_list_area: Rect,
    pub(crate) detail_area: Rect,
    pub(crate) item_areas: Vec<(usize, Rect)>,
    pub(crate) transcript_item_ranges: Vec<(usize, std::ops::Range<usize>)>,
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
        let message_history = load_history(&message_history_path)?;
        let command_history = load_history(&command_history_path)?;
        Ok(Self {
            endpoint,
            message_history_path,
            command_history_path,
            threads,
            models,
            thread_id,
            transcript,
            events,
            tool_call_preview: None,
            message_input: InputBuffer::new(message_history, true),
            command_input: InputBuffer::new(command_history, false),
            command_open: false,
            help_open: false,
            word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
            activity: Some(Activity::Info(if login_required {
                "Codex login required · Ctrl-L login".to_owned()
            } else {
                "/thread · /new · /model · Ctrl-P/Ctrl-/ command · Tab focus · Ctrl-C copies"
                    .to_owned()
            })),
            approval: None,
            renaming: false,
            model_picker: None,
            thread_picker: None,
            new_thread_model: None,
            login_required,
            selection_start: None,
            selection_end: None,
            transcript_layout: TranscriptLayout { rows: Vec::new() },
            turn_pending: false,
            metrics_stale: false,
            transcript_mode: TranscriptMode::Coding,
            focus: FocusPane::Input,
            transcript_scroll: 0,
            detail_scroll: 0,
            selected_request: None,
            raw_request: false,
            expanded_tools: HashSet::new(),
            selected_item: None,
            transcript_area: Rect::default(),
            transcript_scrollbar_area: Rect::default(),
            transcript_max_scroll: 0,
            transcript_scrollbar_thumb_start: 0,
            transcript_scrollbar_thumb_len: 0,
            transcript_scrollbar_drag_offset: None,
            input_area: Rect::default(),
            request_list_area: Rect::default(),
            detail_area: Rect::default(),
            item_areas: Vec::new(),
            transcript_item_ranges: Vec::new(),
        })
    }

    pub(super) async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        let mut events = EventStream::new();
        let (turns, mut completed_turns) = mpsc::unbounded_channel();
        let mut redraw = tokio::time::interval(Duration::from_millis(16));
        redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        terminal.draw(|frame| self.render(frame))?;
        redraw.tick().await;
        let mut dirty = false;
        loop {
            tokio::select! {
                event = events.next() => {
                    let Some(event) = event.transpose()? else {
                        return Ok(());
                    };
                    match event {
                        Event::Key(key) if self.handle_key(key, &turns).await? => return Ok(()),
                        Event::Mouse(mouse) => self.handle_mouse(mouse).await?,
                        Event::Resize(_, _) => {}
                        _ => {}
                    }
                    dirty = true;
                }
                Some(completion) = completed_turns.recv() => {
                    self.update_turn(completion)?;
                    dirty = true;
                }
                _ = redraw.tick() => {
                    if dirty {
                        terminal.draw(|frame| self.render(frame))?;
                        dirty = false;
                    }
                }
            }
        }
    }

    async fn handle_key(
        &mut self,
        key: KeyEvent,
        turns: &mpsc::UnboundedSender<TurnUpdate>,
    ) -> Result<bool> {
        if self.help_open {
            let toggle = key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('/'));
            if toggle || matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                self.help_open = false;
                self.activity = None;
            }
            return Ok(false);
        }

        if self.command_open {
            let toggle = key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('/'));
            if toggle || key.code == KeyCode::Esc {
                self.command_open = false;
                self.command_input.clear();
                self.activity = None;
                return Ok(false);
            }
            if matches!(
                self.command_input.handle_key(key, &self.word_segmenter),
                InputAction::Submit
            ) {
                self.command_open = false;
                let command = self.command_input.take();
                return self.execute_command(&command).await;
            }
            return Ok(false);
        }

        if let Some(approval) = &mut self.approval {
            if let Some(reason) = &mut approval.deny_reason {
                match key.code {
                    KeyCode::Enter => {
                        let id = approval.id;
                        let reason = reason.take();
                        let reason = (!reason.trim().is_empty()).then_some(reason);
                        self.resolve_approval(id, false, reason, turns);
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
                        self.resolve_approval(id, true, None, turns);
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
            if matches!(key.code, KeyCode::Char('p') | KeyCode::Char('/'))
                && self.approval.is_none()
                && !self.renaming
                && self.model_picker.is_none()
                && self.thread_picker.is_none()
                && !self.help_open
            {
                self.command_input.clear();
                self.command_open = true;
                return Ok(false);
            }
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('g'))
                && self.approval.is_none()
                && !self.renaming
                && self.model_picker.is_none()
                && self.thread_picker.is_none()
                && self.focus == FocusPane::Input
                && !self.message_input.value.trim().is_empty()
            {
                if self.message_input.value.starts_with('/') {
                    let command = self.message_input.take();
                    return self.execute_command(&command).await;
                } else if !self.turn_pending {
                    self.send(turns)?;
                }
                return Ok(false);
            }
            match key.code {
                KeyCode::Char('r') if self.thread_id.is_some() => {
                    self.renaming = true;
                    self.model_picker = None;
                    self.thread_picker = None;
                    self.focus = FocusPane::Input;
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
                    match request(&self.endpoint, ControllerRequest::CodexLogin).await? {
                        ControllerResponse::CodexLoggedIn { .. } => {
                            self.login_required = false;
                            self.activity = Some(Activity::Info("Codex login complete".to_owned()));
                        }
                        ControllerResponse::Error { message } => {
                            self.activity = Some(Activity::Error(sanitize(&message)));
                        }
                        response => {
                            bail!("controller returned an unexpected response: {response:?}")
                        }
                    }
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
                        Self::record_history(
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

        if self.renaming {
            match key.code {
                KeyCode::Enter if !self.message_input.value.trim().is_empty() => {
                    self.rename().await?
                }
                KeyCode::Esc => {
                    self.renaming = false;
                    self.message_input.clear();
                    self.activity = None;
                }
                _ => {
                    self.message_input.handle_key(key, &self.word_segmenter);
                }
            }
            return Ok(false);
        }

        if let Some(picker) = &mut self.model_picker {
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
                KeyCode::Enter if picker.selecting_effort => self.change_model().await?,
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
                        self.model_picker = None;
                        self.activity = None;
                    }
                }
                _ => {}
            }
            return Ok(false);
        }

        if let Some(picker) = &mut self.thread_picker {
            match key.code {
                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                KeyCode::Down => {
                    picker.selected = (picker.selected + 1).min(self.threads.len() - 1);
                }
                KeyCode::Enter => {
                    let thread_id = self.threads[picker.selected].id;
                    self.thread_picker = None;
                    self.select_thread(thread_id).await?;
                }
                KeyCode::Esc => {
                    self.thread_picker = None;
                    self.activity = None;
                }
                _ => {}
            }
            return Ok(false);
        }

        if self.focus != FocusPane::Input {
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

    async fn execute_command(&mut self, command: &str) -> Result<bool> {
        let command = command.strip_prefix('/').unwrap_or(command);
        if !command.is_empty() {
            Self::record_history(
                &self.command_history_path,
                &mut self.command_input,
                command.to_owned(),
            )?;
        }
        match command {
            "view" => {
                self.transcript_mode = match self.transcript_mode {
                    TranscriptMode::Coding => TranscriptMode::Debug,
                    TranscriptMode::Debug => TranscriptMode::Coding,
                };
                self.focus = FocusPane::Input;
                self.clear_selection();
                self.activity = Some(Activity::Info(
                    match self.transcript_mode {
                        TranscriptMode::Coding => "Coding transcript",
                        TranscriptMode::Debug => "LLM request inspector",
                    }
                    .to_owned(),
                ));
            }
            "thread" => self.open_thread_picker(),
            "new" => self.start_new_thread(),
            "model" => self.open_model_picker()?,
            "help" => {
                self.help_open = true;
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

    fn start_new_thread(&mut self) {
        self.thread_id = None;
        self.transcript.clear();
        self.events.clear();
        self.tool_call_preview = None;
        self.message_input.clear();
        self.approval = None;
        self.renaming = false;
        self.model_picker = None;
        self.thread_picker = None;
        self.new_thread_model = None;
        self.clear_selection();
        self.reset_view();
        self.metrics_stale = false;
        self.activity = Some(Activity::Info("New thread".to_owned()));
    }

    fn open_model_picker(&mut self) -> Result<()> {
        self.renaming = false;
        self.thread_picker = None;
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
        self.model_picker = Some(ModelPicker {
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
        self.renaming = false;
        self.model_picker = None;
        if self.threads.is_empty() {
            self.activity = Some(Activity::Info("No threads are available".to_owned()));
            return;
        }
        let selected = self
            .threads
            .iter()
            .position(|thread| Some(thread.id) == self.thread_id)
            .unwrap_or(0);
        self.thread_picker = Some(ThreadPicker { selected });
        self.activity = Some(Activity::Info(
            "Select thread · Enter switches · Esc cancels".to_owned(),
        ));
    }

    async fn select_thread(&mut self, thread_id: i64) -> Result<()> {
        self.thread_id = Some(thread_id);
        (self.transcript, self.events) = load_transcript(&self.endpoint, thread_id).await?;
        self.tool_call_preview = None;
        self.approval = None;
        self.renaming = false;
        self.model_picker = None;
        self.clear_selection();
        self.reset_view();
        self.metrics_stale = false;
        self.activity = Some(Activity::Info("Thread selected".to_owned()));
        Ok(())
    }

    async fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if self.help_open || self.thread_picker.is_some() {
            return Ok(());
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if self
                    .transcript_scrollbar_area
                    .contains((mouse.column, mouse.row).into()) =>
            {
                self.focus = FocusPane::Transcript;
                let position = mouse.row.saturating_sub(self.transcript_scrollbar_area.y);
                if position == 0 {
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_add(1)
                        .min(self.transcript_max_scroll);
                } else if position == self.transcript_scrollbar_area.height.saturating_sub(1) {
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(1);
                } else {
                    let track_position = position - 1;
                    let thumb_end = self
                        .transcript_scrollbar_thumb_start
                        .saturating_add(self.transcript_scrollbar_thumb_len);
                    let drag_offset = if track_position >= self.transcript_scrollbar_thumb_start
                        && track_position < thumb_end
                    {
                        track_position - self.transcript_scrollbar_thumb_start
                    } else {
                        self.transcript_scrollbar_thumb_len / 2
                    };
                    self.transcript_scrollbar_drag_offset = Some(drag_offset);
                    self.drag_transcript_scrollbar(track_position);
                }
                return Ok(());
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self.transcript_scrollbar_drag_offset.is_some() =>
            {
                let track_position = mouse
                    .row
                    .saturating_sub(self.transcript_scrollbar_area.y + 1)
                    .min(self.transcript_scrollbar_area.height.saturating_sub(3));
                self.drag_transcript_scrollbar(track_position);
                return Ok(());
            }
            MouseEventKind::Up(MouseButton::Left)
                if self.transcript_scrollbar_drag_offset.is_some() =>
            {
                self.transcript_scrollbar_drag_offset = None;
                return Ok(());
            }
            _ => {}
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.detail_area.contains((mouse.column, mouse.row).into()) {
                    self.detail_scroll = self.detail_scroll.saturating_sub(3);
                    self.focus = FocusPane::Detail;
                } else {
                    self.transcript_scroll = self.transcript_scroll.saturating_add(3);
                    self.focus = FocusPane::Transcript;
                }
                return Ok(());
            }
            MouseEventKind::ScrollDown => {
                if self.detail_area.contains((mouse.column, mouse.row).into()) {
                    self.detail_scroll = self.detail_scroll.saturating_add(3);
                    self.focus = FocusPane::Detail;
                } else {
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(3);
                    self.focus = FocusPane::Transcript;
                }
                return Ok(());
            }
            _ => {}
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.input_area.contains((mouse.column, mouse.row).into())
        {
            self.focus = FocusPane::Input;
            return Ok(());
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self
                .request_list_area
                .contains((mouse.column, mouse.row).into())
        {
            let row = usize::from(mouse.row.saturating_sub(self.request_list_area.y + 1)) / 3;
            if row < self.request_count() {
                self.selected_request = Some(row);
                self.detail_scroll = 0;
            }
            self.focus = FocusPane::Requests;
            return Ok(());
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.detail_area.contains((mouse.column, mouse.row).into())
        {
            self.focus = FocusPane::Detail;
            return Ok(());
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some((index, _)) = self
                .item_areas
                .iter()
                .find(|(_, area)| area.contains((mouse.column, mouse.row).into()))
        {
            let index = *index;
            if self.selected_item == Some(index)
                && self.transcript[index].is_tool_result()
                && !self.expanded_tools.remove(&index)
            {
                self.expanded_tools.insert(index);
            }
            self.selected_item = Some(index);
            self.focus = FocusPane::Transcript;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.transcript_mode == TranscriptMode::Coding
                    && self
                        .transcript_area
                        .contains((mouse.column, mouse.row).into())
                {
                    self.focus = FocusPane::Transcript;
                }
                self.selection_start = self.point_at(mouse.column, mouse.row);
                self.selection_end = self.selection_start;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.selection_start.is_some() {
                    self.selection_end = self.point_at(mouse.column, mouse.row);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn drag_transcript_scrollbar(&mut self, track_position: u16) {
        let movable_height = self
            .transcript_scrollbar_area
            .height
            .saturating_sub(2)
            .saturating_sub(self.transcript_scrollbar_thumb_len);
        let thumb_start = track_position
            .saturating_sub(self.transcript_scrollbar_drag_offset.unwrap())
            .min(movable_height);
        let scroll = if movable_height == 0 {
            0
        } else {
            self.transcript_max_scroll
                .saturating_mul(usize::from(thumb_start))
                .saturating_add(usize::from(movable_height) / 2)
                / usize::from(movable_height)
        };
        self.transcript_scroll = self.transcript_max_scroll.saturating_sub(scroll);
    }

    fn send(&mut self, turns: &mpsc::UnboundedSender<TurnUpdate>) -> Result<()> {
        let message = self.message_input.take();
        Self::record_history(
            &self.message_history_path,
            &mut self.message_input,
            message.clone(),
        )?;
        self.transcript
            .push(TranscriptEntry::message(Author::User, sanitize(&message)));
        self.turn_pending = true;
        self.activity = Some(Activity::Info("Waiting for Atra Controller…".to_owned()));
        let endpoint = self.endpoint.clone();
        let existing_thread_id = self.thread_id;
        let new_thread_model = self.new_thread_model.take();
        let turns = turns.clone();
        tokio::spawn(async move {
            let result = async {
                let thread_id = match existing_thread_id {
                    Some(thread_id) => thread_id,
                    None => {
                        let thread_id = match request(
                            &endpoint,
                            ControllerRequest::ThreadCreate { display_name: None },
                        )
                        .await?
                        {
                            ControllerResponse::ThreadCreated { thread_id } => thread_id,
                            ControllerResponse::Error { message } => bail!("{message}"),
                            response => {
                                bail!("controller returned an unexpected response: {response:?}")
                            }
                        };
                        if let Some((model, reasoning_effort)) = new_thread_model {
                            match request(
                                &endpoint,
                                ControllerRequest::ThreadSetModel {
                                    thread_id,
                                    model,
                                    reasoning_effort,
                                },
                            )
                            .await?
                            {
                                ControllerResponse::ThreadModelChanged => {}
                                ControllerResponse::Error { message } => bail!("{message}"),
                                response => bail!(
                                    "controller returned an unexpected response: {response:?}"
                                ),
                            }
                        }
                        let threads =
                            match request(&endpoint, ControllerRequest::ThreadList).await? {
                                ControllerResponse::ThreadList { threads } => threads,
                                ControllerResponse::Error { message } => bail!("{message}"),
                                response => bail!(
                                    "controller returned an unexpected response: {response:?}"
                                ),
                            };
                        turns
                            .send(TurnUpdate::Started {
                                message: message.clone(),
                                thread_id,
                                threads,
                            })
                            .ok();
                        thread_id
                    }
                };
                let response = request_stream(
                    &endpoint,
                    ControllerRequest::ThreadSend {
                        thread_id,
                        message: message.clone(),
                    },
                    thread_id,
                    &turns,
                )
                .await?;
                Ok(TurnCompletion {
                    thread_id,
                    response,
                })
            }
            .await;
            let _ = turns.send(TurnUpdate::Completed(result));
        });
        Ok(())
    }

    fn record_history(
        history_path: &Path,
        input_buffer: &mut InputBuffer,
        input: String,
    ) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(history_path)
            .with_context(|| format!("failed to open TUI history {}", history_path.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to set TUI history permissions {}",
                    history_path.display()
                )
            })?;
        let mut line = serde_json::to_vec(&input).context("failed to encode TUI history")?;
        line.push(b'\n');
        file.write_all(&line)
            .with_context(|| format!("failed to write TUI history {}", history_path.display()))?;
        input_buffer.record_history(input);
        Ok(())
    }

    async fn rename(&mut self) -> Result<()> {
        let thread_id = self.thread_id.context("no thread is selected")?;
        let display_name = self.message_input.take();
        match request(
            &self.endpoint,
            ControllerRequest::ThreadRename {
                thread_id,
                display_name: display_name.clone(),
            },
        )
        .await?
        {
            ControllerResponse::ThreadRenamed => {
                if let Some(thread) = self
                    .threads
                    .iter_mut()
                    .find(|thread| thread.id == thread_id)
                {
                    thread.display_name = Some(display_name);
                }
                self.renaming = false;
                self.activity = Some(Activity::Info("Thread renamed".to_owned()));
                Ok(())
            }
            ControllerResponse::Error { message } => bail!("{message}"),
            response => bail!("controller returned an unexpected response: {response:?}"),
        }
    }

    async fn change_model(&mut self) -> Result<()> {
        let picker = self
            .model_picker
            .as_ref()
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
            self.model_picker = None;
            self.metrics_stale = true;
            self.activity = Some(Activity::Info("Model selected for new thread".to_owned()));
            return Ok(());
        };
        match request(
            &self.endpoint,
            ControllerRequest::ThreadSetModel {
                thread_id,
                model: model.clone(),
                reasoning_effort: reasoning_effort.clone(),
            },
        )
        .await?
        {
            ControllerResponse::ThreadModelChanged => {
                if let Some(thread) = self
                    .threads
                    .iter_mut()
                    .find(|thread| thread.id == thread_id)
                {
                    thread.model = model;
                    thread.reasoning_effort = reasoning_effort;
                }
                self.model_picker = None;
                self.metrics_stale = true;
                self.activity = Some(Activity::Info("Thread model changed".to_owned()));
                Ok(())
            }
            ControllerResponse::Error { message } => bail!("{message}"),
            response => bail!("controller returned an unexpected response: {response:?}"),
        }
    }

    fn resolve_approval(
        &mut self,
        approval_id: u64,
        allowed: bool,
        reason: Option<String>,
        turns: &mpsc::UnboundedSender<TurnUpdate>,
    ) {
        let request_message = if allowed {
            ControllerRequest::ApprovalAllow { approval_id }
        } else {
            ControllerRequest::ApprovalDeny {
                approval_id,
                reason,
            }
        };
        self.approval = None;
        self.turn_pending = true;
        self.activity = Some(Activity::Info("Waiting for Atra Controller…".to_owned()));
        let endpoint = self.endpoint.clone();
        let thread_id = self.thread_id.expect("approval belongs to a thread");
        let turns = turns.clone();
        tokio::spawn(async move {
            let result = request_stream(&endpoint, request_message, thread_id, &turns)
                .await
                .map(|response| TurnCompletion {
                    thread_id,
                    response,
                });
            let _ = turns.send(TurnUpdate::Completed(result));
        });
    }

    fn update_turn(&mut self, update: TurnUpdate) -> Result<()> {
        let completion = match update {
            TurnUpdate::Started {
                message,
                thread_id,
                threads,
            } => {
                self.threads = threads;
                if self.thread_id.is_none()
                    && self.transcript.last().is_some_and(|item| {
                        matches!(
                            &item.item,
                            TranscriptItem::Message {
                                author: Author::User,
                                text,
                            } if text == &sanitize(&message)
                        )
                    })
                {
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
                    self.tool_call_preview = Some((item_id, index));
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
                    let Some(item) = event_to_item(event) else {
                        return Ok(());
                    };
                    if matches!(item, TranscriptItem::ToolCall { .. })
                        && let Some(item_id) = item_id
                        && let Some((preview_id, index)) = self.tool_call_preview.take()
                        && preview_id == item_id
                    {
                        self.transcript[index].replace(item);
                        return Ok(());
                    }
                    push_transcript_item(&mut self.transcript, item);
                }
                return Ok(());
            }
            TurnUpdate::Completed(Ok(completion)) => completion,
            TurnUpdate::Completed(Err(error)) => {
                if let Some((_, index)) = self.tool_call_preview.take() {
                    self.transcript.remove(index);
                }
                self.turn_pending = false;
                self.activity = Some(Activity::Error(sanitize(&format!("{error:#}"))));
                return Ok(());
            }
        };
        self.turn_pending = false;
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
                self.approval = Some(Approval {
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
            .transcript_layout
            .rows
            .iter()
            .find(|line| line.y == row)?;
        let index = usize::from(column.saturating_sub(mapped.x));
        Some(SelectionPoint {
            offset: mapped.cells.get(index).copied().unwrap_or(mapped.end),
        })
    }

    pub(crate) fn selection_range(&self) -> Option<(usize, usize)> {
        let start = self.selection_start?.offset;
        let end = self.selection_end?.offset;
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
        self.selection_start = None;
        self.selection_end = None;
    }

    fn reset_view(&mut self) {
        self.transcript_scroll = 0;
        self.detail_scroll = 0;
        self.selected_request = None;
        self.raw_request = false;
        self.expanded_tools.clear();
        self.selected_item = None;
        self.focus = FocusPane::Input;
    }

    fn cycle_focus(&mut self, reverse: bool) {
        let panes: &[FocusPane] = match self.transcript_mode {
            TranscriptMode::Coding => &[FocusPane::Input, FocusPane::Transcript],
            TranscriptMode::Debug => &[FocusPane::Input, FocusPane::Requests, FocusPane::Detail],
        };
        let current = panes
            .iter()
            .position(|pane| *pane == self.focus)
            .unwrap_or(0);
        let next = if reverse {
            current.checked_sub(1).unwrap_or(panes.len() - 1)
        } else {
            (current + 1) % panes.len()
        };
        self.focus = panes[next];
    }

    fn handle_pane_key(&mut self, key: KeyEvent) {
        match self.focus {
            FocusPane::Transcript => match key.code {
                KeyCode::Home => self.transcript_scroll = self.transcript_max_scroll,
                KeyCode::PageUp => {
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_add(usize::from(self.transcript_area.height))
                }
                KeyCode::PageDown => {
                    self.transcript_scroll = self
                        .transcript_scroll
                        .saturating_sub(usize::from(self.transcript_area.height))
                }
                KeyCode::Up => self.select_item(false),
                KeyCode::Down => self.select_item(true),
                KeyCode::Enter => {
                    if let Some(index) = self.selected_item
                        && self.transcript[index].is_tool_result()
                        && !self.expanded_tools.remove(&index)
                    {
                        self.expanded_tools.insert(index);
                    }
                }
                KeyCode::End => self.transcript_scroll = 0,
                _ => {}
            },
            FocusPane::Requests => match key.code {
                KeyCode::Up => {
                    let selected = self.selected_request.unwrap_or(self.request_count());
                    self.selected_request = Some(selected.saturating_sub(1));
                    self.detail_scroll = 0;
                }
                KeyCode::Down => {
                    let last = self.request_count().saturating_sub(1);
                    self.selected_request = Some(
                        self.selected_request
                            .unwrap_or(last)
                            .saturating_add(1)
                            .min(last),
                    );
                    self.detail_scroll = 0;
                }
                _ => {}
            },
            FocusPane::Detail => match key.code {
                KeyCode::Up => self.detail_scroll = self.detail_scroll.saturating_sub(1),
                KeyCode::Down => self.detail_scroll = self.detail_scroll.saturating_add(1),
                KeyCode::PageUp => {
                    self.detail_scroll = self
                        .detail_scroll
                        .saturating_sub(usize::from(self.detail_area.height))
                }
                KeyCode::PageDown => {
                    self.detail_scroll = self
                        .detail_scroll
                        .saturating_add(usize::from(self.detail_area.height))
                }
                KeyCode::Char('r') => {
                    self.raw_request = !self.raw_request;
                    self.detail_scroll = 0;
                }
                KeyCode::End => self.detail_scroll = 0,
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
            self.selected_item = None;
            return;
        }
        let next = match (self.selected_item, forward) {
            (Some(index), true) => (index + 1).min(self.transcript.len() - 1),
            (Some(index), false) => index.saturating_sub(1),
            (None, true) => 0,
            (None, false) => self.transcript.len() - 1,
        };
        self.selected_item = Some(next);
        let scroll = self
            .transcript_item_ranges
            .iter()
            .find_map(|(index, rows)| (*index == next).then_some(rows.start));
        if let Some(item_start) = scroll {
            let viewport_start = self
                .transcript_max_scroll
                .saturating_sub(self.transcript_scroll);
            let viewport_end =
                viewport_start + usize::from(self.transcript_area.height.saturating_sub(2));
            if item_start < viewport_start || item_start >= viewport_end {
                self.transcript_scroll = self
                    .transcript_max_scroll
                    .saturating_sub(item_start.min(self.transcript_max_scroll));
            }
        }
    }
}

pub(crate) fn layout_transcript(
    entries: &[TranscriptEntry],
    area: Rect,
    scroll: usize,
) -> TranscriptLayout {
    let mut rows = Vec::new();
    let mut virtual_y = 0;
    let mut offset = 0;
    for (item_index, entry) in entries.iter().enumerate() {
        let mut first_line = true;
        for displayed in &entry.rendered.as_ref().unwrap().lines {
            if !first_line && !displayed.continuation {
                offset += 1;
            }
            first_line = false;
            let line = &displayed.line;
            let content_start = offset;
            let content_len = line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>();
            if virtual_y >= scroll && virtual_y - scroll < usize::from(area.height) {
                let mut cells = Vec::new();
                let mut span_start = content_start;
                for span in &line.spans {
                    for (byte, character) in span.content.char_indices() {
                        let character_width = character.width().unwrap_or(0);
                        cells.extend(std::iter::repeat_n(span_start + byte, character_width));
                    }
                    span_start += span.content.len();
                }
                rows.push(MappedRow {
                    x: area.x + 2,
                    y: area.y + (virtual_y - scroll) as u16,
                    cells,
                    end: content_start + content_len,
                });
            }
            offset += content_len;
            virtual_y += 1;
        }
        offset += 1;
        if entries.get(item_index + 1).is_none_or(|next| {
            !matches!(entry.item, TranscriptItem::ToolCall { .. }) || !next.is_tool_result()
        }) {
            virtual_y += 1;
        }
    }
    TranscriptLayout { rows }
}

fn transcript_text(entries: &[TranscriptEntry]) -> String {
    let mut text = String::new();
    for entry in entries {
        let mut first_line = true;
        for displayed in &entry.rendered.as_ref().unwrap().lines {
            if !first_line && !displayed.continuation {
                text.push('\n');
            }
            first_line = false;
            for span in &displayed.line.spans {
                text.push_str(&span.content);
            }
        }
        text.push('\n');
    }
    text
}

#[derive(Clone)]
struct AtraMarkdownStyle;

impl StyleSheet for AtraMarkdownStyle {
    fn heading(&self, level: u8) -> Style {
        match level {
            1 => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            2 => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            _ => Style::default().add_modifier(Modifier::BOLD),
        }
    }

    fn code(&self) -> Style {
        Style::default().fg(Color::LightCyan)
    }

    fn link(&self) -> Style {
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::UNDERLINED)
    }

    fn blockquote(&self) -> Style {
        Style::default().fg(Color::Green)
    }

    fn heading_meta(&self) -> Style {
        Style::default().fg(Color::DarkGray)
    }

    fn metadata_block(&self) -> Style {
        Style::default().fg(Color::Yellow)
    }

    fn code_block_fence(&self) -> &str {
        ""
    }
}

pub(crate) fn transcript_lines(
    entries: &[TranscriptEntry],
    selection: Option<(usize, usize)>,
    selected_item: Option<usize>,
    width: u16,
    visible: std::ops::Range<usize>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut offset = 0;
    let mut row = 0;
    for (item_index, entry) in entries.iter().enumerate() {
        let item = &entry.item;
        let mut first_line = true;
        for displayed in &entry.rendered.as_ref().unwrap().lines {
            if !first_line && !displayed.continuation {
                offset += 1;
            }
            first_line = false;
            let DisplayedLine {
                marker,
                line,
                continuation: _,
            } = displayed;
            let line_len = line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>();
            if visible.contains(&row) {
                let gutter_style = if item.is_user_message() {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };
                let mut spans = vec![
                    Span::styled(
                        marker.unwrap_or(' ').to_string(),
                        marker_style(item, selected_item == Some(item_index)),
                    ),
                    Span::styled(" ", gutter_style),
                ];
                spans.extend(highlight_selection(line.clone(), offset, selection).spans);
                if item.is_user_message() {
                    let rendered_width = spans.iter().map(Span::width).sum::<usize>();
                    spans.push(Span::styled(
                        " ".repeat(usize::from(width).saturating_sub(rendered_width)),
                        Style::default().bg(Color::DarkGray),
                    ));
                }
                lines.push(Line::from(spans));
            }
            offset += line_len;
            row += 1;
        }
        if entries.get(item_index + 1).is_none_or(|next| {
            !matches!(item, TranscriptItem::ToolCall { .. }) || !next.is_tool_result()
        }) {
            if visible.contains(&row) {
                lines.push(Line::default());
            }
            row += 1;
        }
        offset += 1;
    }
    lines
}

pub(crate) fn transcript_ranges(
    entries: &[TranscriptEntry],
) -> (usize, Vec<(usize, std::ops::Range<usize>)>) {
    let mut row = 0;
    let mut ranges = Vec::with_capacity(entries.len());
    for (item_index, entry) in entries.iter().enumerate() {
        let start = row;
        row += entry.rendered.as_ref().unwrap().lines.len();
        ranges.push((item_index, start..row));
        if entries.get(item_index + 1).is_none_or(|next| {
            !matches!(entry.item, TranscriptItem::ToolCall { .. }) || !next.is_tool_result()
        }) {
            row += 1;
        }
    }
    (row, ranges)
}

pub(crate) fn prepare_transcript(
    entries: &mut [TranscriptEntry],
    expanded_tools: &HashSet<usize>,
    width: u16,
) {
    for (item_index, entry) in entries.iter_mut().enumerate() {
        let expanded = expanded_tools.contains(&item_index);
        if entry
            .rendered
            .as_ref()
            .is_some_and(|rendered| rendered.width == width && rendered.expanded == expanded)
        {
            continue;
        }
        entry.rendered = Some(RenderedItem {
            width,
            expanded,
            lines: displayed_item_lines(&entry.item, expanded, width),
        });
    }
}

fn displayed_item_lines(item: &TranscriptItem, expanded: bool, width: u16) -> Vec<DisplayedLine> {
    let content_width = usize::from(width.saturating_sub(2)).max(1);
    if let TranscriptItem::Message { author, text } = item {
        let background = (*author == Author::User).then_some(Color::DarkGray);
        let mut first_event_line = true;
        return render_markdown(text)
            .into_iter()
            .flat_map(|line| {
                wrap_line(line, content_width)
                    .into_iter()
                    .enumerate()
                    .collect::<Vec<_>>()
            })
            .map(|(wrap_index, mut line)| {
                if let Some(background) = background {
                    line.style = line.style.bg(background);
                    for span in &mut line.spans {
                        span.style = span.style.bg(background);
                    }
                }
                let displayed = DisplayedLine {
                    marker: first_event_line.then_some('•'),
                    line,
                    continuation: wrap_index != 0,
                };
                first_event_line = false;
                displayed
            })
            .collect();
    }
    let mut logical_lines = match item {
        TranscriptItem::ToolCall { name, arguments } => tool_call_lines(name, arguments.as_ref()),
        TranscriptItem::ToolResult { result } => {
            let result = format_tool_value(result);
            let display = if expanded {
                result
            } else {
                summarize_result(&result)
            };
            display
                .lines()
                .map(|line| (None, Line::from(line.to_owned())))
                .collect()
        }
        TranscriptItem::Approval { tool, allowed, .. } => {
            let decision = allowed.map(|allowed| if allowed { "approved" } else { "denied" });
            let message = match (tool, decision) {
                (Some(tool), Some(decision)) => format!("{tool} {decision}"),
                (Some(tool), None) => format!("{tool} approval"),
                (None, Some(decision)) => decision.to_owned(),
                (None, None) => unreachable!(),
            };
            vec![(
                Some(match allowed {
                    Some(true) => '✓',
                    Some(false) => '✗',
                    None => '?',
                }),
                Line::from(message),
            )]
        }
        TranscriptItem::Message { .. } => unreachable!(),
    };
    if logical_lines.is_empty() {
        logical_lines.push((None, Line::default()));
    }
    logical_lines
        .into_iter()
        .flat_map(|(marker, line)| {
            wrap_line(line, content_width)
                .into_iter()
                .enumerate()
                .map(move |(index, line)| DisplayedLine {
                    marker: (index == 0).then_some(marker).flatten(),
                    line,
                    continuation: index != 0,
                })
        })
        .collect()
}

fn marker_style(item: &TranscriptItem, selected: bool) -> Style {
    let style = match item {
        TranscriptItem::Message {
            author: Author::User,
            ..
        } => Style::default().fg(Color::Cyan).bg(Color::DarkGray),
        TranscriptItem::Message {
            author: Author::Assistant,
            ..
        } => Style::default().fg(Color::Cyan),
        TranscriptItem::ToolCall { .. } => Style::default().fg(Color::Yellow),
        TranscriptItem::ToolResult { .. } => Style::default().fg(Color::DarkGray),
        TranscriptItem::Approval { allowed: None, .. } => Style::default().fg(Color::Yellow),
        TranscriptItem::Approval {
            allowed: Some(true),
            ..
        } => Style::default().fg(Color::Green),
        TranscriptItem::Approval {
            allowed: Some(false),
            ..
        } => Style::default().fg(Color::Red),
    };
    if selected {
        style.add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        style
    }
}

fn tool_call_lines(
    name: &str,
    arguments: Option<&serde_json::Value>,
) -> Vec<(Option<char>, Line<'static>)> {
    let object = arguments.and_then(serde_json::Value::as_object);
    match name {
        "exec_command" => {
            let runner = object
                .and_then(|arguments| arguments.get("runner"))
                .and_then(serde_json::Value::as_str);
            let cwd = object
                .and_then(|arguments| arguments.get("cwd"))
                .and_then(serde_json::Value::as_str);
            let location = match (cwd, runner) {
                (Some(cwd), Some(runner)) => format!("{cwd} · {runner}"),
                (Some(cwd), None) => cwd.to_owned(),
                (None, Some(runner)) => runner.to_owned(),
                (None, None) => ".".to_owned(),
            };
            let command = object
                .and_then(|arguments| arguments.get("command"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let mut lines = vec![(Some('┌'), Line::from(location))];
            lines.extend(
                bash_lines(command)
                    .into_iter()
                    .enumerate()
                    .map(|(index, line)| ((index == 0).then_some('$'), line)),
            );
            lines
        }
        "apply_patch" => {
            let input = arguments
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let runner = input
                .lines()
                .find_map(|line| line.strip_prefix("*** Environment ID: "));
            let patch = input
                .lines()
                .filter(|line| {
                    !line.starts_with("*** Begin Patch")
                        && !line.starts_with("*** Environment ID:")
                        && !line.starts_with("*** End Patch")
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut lines = runner
                .map(|runner| (Some('┌'), Line::from(runner.to_owned())))
                .into_iter()
                .collect::<Vec<_>>();
            lines.push((Some('±'), Line::from("apply patch")));
            lines.extend(patch_lines(&patch).into_iter().map(|line| (None, line)));
            lines
        }
        "list_runners" => vec![(Some('◆'), Line::from("list runners"))],
        "wait_process" => vec![(
            Some('…'),
            Line::from(format!(
                "wait process #{}",
                tool_argument(object, "process_handle")
            )),
        )],
        "write_process" => vec![
            (
                Some('›'),
                Line::from(format!(
                    "write process #{}",
                    tool_argument(object, "process_handle")
                )),
            ),
            (
                None,
                Line::from(
                    object
                        .and_then(|arguments| arguments.get("input"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
            ),
        ],
        "stop_process" => vec![(
            Some('■'),
            Line::from(format!(
                "stop process #{}",
                tool_argument(object, "process_handle")
            )),
        )],
        _ => {
            let mut lines = vec![(
                Some('◆'),
                Line::from(if arguments.is_some() {
                    name.to_owned()
                } else {
                    format!("{name}…")
                }),
            )];
            if let Some(arguments) = object {
                lines.extend(arguments.iter().map(|(key, value)| {
                    (
                        None,
                        Line::from(format!("{key}: {}", format_tool_value(value))),
                    )
                }));
            }
            lines
        }
    }
}

fn bash_lines(command: &str) -> Vec<Line<'static>> {
    static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
    static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

    let syntax = SYNTAX_SET
        .find_syntax_by_token("bash")
        .expect("syntect includes bash syntax");
    let mut highlighter = HighlightLines::new(syntax, &THEME_SET.themes["base16-ocean.dark"]);
    let mut lines = LinesWithEndings::from(command)
        .map(|line| {
            let spans = highlighter
                .highlight_line(line, &SYNTAX_SET)
                .expect("bundled bash syntax is valid")
                .into_iter()
                .filter_map(|(style, text)| {
                    let text = text.trim_end_matches(['\r', '\n']);
                    (!text.is_empty()).then(|| {
                        let mut ratatui_style = Style::default().fg(Color::Rgb(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        ));
                        if style.font_style.contains(FontStyle::BOLD) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
                        }
                        if style.font_style.contains(FontStyle::ITALIC) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::ITALIC);
                        }
                        if style.font_style.contains(FontStyle::UNDERLINE) {
                            ratatui_style = ratatui_style.add_modifier(Modifier::UNDERLINED);
                        }
                        Span::styled(text.to_owned(), ratatui_style)
                    })
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut rows = vec![Line::default()];
    let mut row_width = 0;
    for span in line.spans {
        let mut chunk = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or(0);
            if row_width > 0 && row_width + character_width > width {
                if !chunk.is_empty() {
                    rows.last_mut()
                        .unwrap()
                        .spans
                        .push(Span::styled(std::mem::take(&mut chunk), span.style));
                }
                rows.push(Line::default());
                row_width = 0;
            }
            chunk.push(character);
            row_width += character_width;
        }
        if !chunk.is_empty() {
            rows.last_mut()
                .unwrap()
                .spans
                .push(Span::styled(chunk, span.style));
        }
    }
    for row in &mut rows {
        row.style = line.style;
        row.alignment = line.alignment;
    }
    rows
}

fn highlight_selection(
    line: Line<'static>,
    offset: usize,
    selection: Option<(usize, usize)>,
) -> Line<'static> {
    let Some((selection_start, selection_end)) = selection else {
        return line;
    };
    let mut span_offset = offset;
    let spans = line
        .spans
        .into_iter()
        .flat_map(|span| {
            let start = span_offset;
            span_offset += span.content.len();
            let mut spans = Vec::new();
            let mut chunk = String::new();
            let mut selected = None;
            for (byte, character) in span.content.char_indices() {
                let character_selected =
                    start + byte >= selection_start && start + byte < selection_end;
                if selected.is_some_and(|selected| selected != character_selected) {
                    let style = if selected.unwrap() {
                        span.style.bg(Color::Blue)
                    } else {
                        span.style
                    };
                    spans.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                selected = Some(character_selected);
                chunk.push(character);
            }
            if let Some(selected) = selected {
                let style = if selected {
                    span.style.bg(Color::Blue)
                } else {
                    span.style
                };
                spans.push(Span::styled(chunk, style));
            }
            spans
        })
        .collect();
    Line {
        style: line.style,
        alignment: line.alignment,
        spans,
    }
}

fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let options = MarkdownOptions::new(AtraMarkdownStyle);
    from_str_with_options(text, &options)
        .lines
        .into_iter()
        .map(|line| Line {
            style: line.style,
            alignment: line.alignment,
            spans: line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.into_owned(), span.style))
                .collect(),
        })
        .collect()
}

fn summarize_result(result: &str) -> String {
    let lines = result.lines().collect::<Vec<_>>();
    if lines.len() <= 5 {
        return result.to_owned();
    }
    format!(
        "{}\n{}\n… {} lines omitted …\n{}\n{}",
        lines[0],
        lines[1],
        lines.len() - 4,
        lines[lines.len() - 2],
        lines[lines.len() - 1]
    )
}

fn patch_lines(patch: &str) -> Vec<Line<'static>> {
    patch
        .lines()
        .map(|line| {
            let style = if line.starts_with("*** Add File:")
                || line.starts_with("*** Update File:")
                || line.starts_with("*** Delete File:")
                || line.starts_with("*** Move to:")
            {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with("@@") {
                Style::default().fg(Color::Magenta)
            } else if line.starts_with('+') {
                Style::default().fg(Color::Green)
            } else if line.starts_with('-') {
                Style::default().fg(Color::Red)
            } else if line.starts_with("***") {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            Line::from(Span::styled(line.to_owned(), style))
        })
        .collect()
}

fn load_history(path: &Path) -> Result<Vec<String>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read TUI history {}", path.display()));
        }
    };
    contents
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to decode TUI history {} at line {}",
                    path.display(),
                    index + 1
                )
            })
        })
        .collect()
}

async fn load_transcript(
    endpoint: &Path,
    thread_id: i64,
) -> Result<(Vec<TranscriptEntry>, Vec<ThreadEvent>)> {
    match request(endpoint, ControllerRequest::ThreadEvents { thread_id }).await? {
        ControllerResponse::ThreadEvents { events } => {
            let mut transcript = Vec::new();
            for item in events.iter().cloned().filter_map(event_to_item) {
                push_transcript_item(&mut transcript, item);
            }
            Ok((transcript, events))
        }
        ControllerResponse::Error { message } => bail!("{message}"),
        response => bail!("controller returned an unexpected response: {response:?}"),
    }
}

fn event_to_item(event: ThreadEvent) -> Option<TranscriptItem> {
    match event.kind.as_str() {
        "user_message" => Some(TranscriptItem::message(
            Author::User,
            sanitize(event.payload.get("content")?.as_str()?),
        )),
        "assistant_message" => Some(TranscriptItem::message(
            Author::Assistant,
            sanitize(event.payload.get("content")?.as_str()?),
        )),
        "tool_call" => Some(TranscriptItem::ToolCall {
            name: sanitize(event.payload.get("name")?.as_str()?),
            arguments: Some(sanitize_value(
                event
                    .payload
                    .get("input")
                    .cloned()
                    .or_else(|| event.payload.get("arguments").cloned())?,
            )),
        }),
        "tool_result" => Some(TranscriptItem::ToolResult {
            result: sanitize_value(event.payload.get("result")?.clone()),
        }),
        "approval_request" => Some(TranscriptItem::Approval {
            id: event.payload.get("approval_id")?.as_u64()?,
            tool: Some(sanitize(event.payload.get("tool")?.as_str()?)),
            allowed: None,
        }),
        "approval_response" => Some(TranscriptItem::Approval {
            id: event.payload.get("approval_id")?.as_u64()?,
            tool: None,
            allowed: Some(event.payload.get("decision")?.as_str()? == "allow"),
        }),
        _ => return None,
    }
}

fn push_transcript_item(transcript: &mut Vec<TranscriptEntry>, item: TranscriptItem) {
    if let TranscriptItem::Approval {
        id,
        tool: None,
        allowed: Some(allowed),
    } = &item
        && let Some(entry) = transcript.last_mut()
        && let TranscriptItem::Approval {
            id: request_id,
            tool: Some(_),
            allowed: request_allowed @ None,
        } = &mut entry.item
        && request_id == id
    {
        *request_allowed = Some(*allowed);
        entry.rendered = None;
        return;
    }
    transcript.push(TranscriptEntry::new(item));
}

fn sanitize_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(value) => serde_json::Value::String(sanitize(&value)),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(sanitize_value).collect())
        }
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (sanitize(&key), sanitize_value(value)))
                .collect(),
        ),
        value => value,
    }
}

fn tool_argument(
    arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> String {
    arguments
        .and_then(|arguments| arguments.get(key))
        .map(format_tool_value)
        .unwrap_or_default()
}

fn format_tool_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        value => serde_json::to_string_pretty(value).unwrap(),
    }
}

pub(crate) fn sanitize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            if characters.next_if_eq(&'[').is_some() {
                for next in characters.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            } else if characters
                .next_if(|next| matches!(next, ']' | 'P' | '^' | '_' | 'X'))
                .is_some()
            {
                while let Some(next) = characters.next() {
                    if next == '\x07' || (next == '\x1b' && characters.next_if_eq(&'\\').is_some())
                    {
                        break;
                    }
                }
            } else {
                characters.next();
            }
        } else if character == '\n' || character == '\t' || !character.is_control() {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
