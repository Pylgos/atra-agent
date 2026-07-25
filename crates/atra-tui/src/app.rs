use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
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
use tokio::sync::mpsc;
use tui_markdown::{Options as MarkdownOptions, StyleSheet, from_str_with_options};
use unicode_width::UnicodeWidthChar;

use crate::controller::{request, request_stream};

#[derive(Clone)]
pub(crate) struct TranscriptItem {
    role: Role,
    text: String,
    markdown: Option<Vec<Line<'static>>>,
}

impl TranscriptItem {
    fn new(role: Role, text: String) -> Self {
        let markdown = matches!(role, Role::User | Role::Assistant).then(|| render_markdown(&text));
        Self {
            role,
            text,
            markdown,
        }
    }

    fn append(&mut self, text: &str) {
        self.text.push_str(text);
        if self.markdown.is_some() {
            self.markdown = Some(render_markdown(&self.text));
        }
    }
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
pub(crate) enum Role {
    User,
    Assistant,
    Tool,
    ToolResult,
    ApprovalRequest,
    ApprovalResponse,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::User => "You",
            Self::Assistant => "Atra",
            Self::Tool => "Tool",
            Self::ToolResult => "Tool result",
            Self::ApprovalRequest => "Approval requested",
            Self::ApprovalResponse => "Approval",
        }
    }
}

pub(crate) struct Approval {
    pub(crate) id: u64,
    pub(crate) description: String,
}

pub(crate) struct ModelPicker {
    pub(crate) models: Vec<Model>,
    pub(crate) model_index: usize,
    pub(crate) effort_index: usize,
    pub(crate) selecting_effort: bool,
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
    text: String,
    rows: Vec<MappedRow>,
}

pub(crate) struct TurnCompletion {
    thread_id: i64,
    response: ControllerResponse,
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
    ToolCallDelta {
        thread_id: i64,
        item_id: String,
        delta: String,
    },
    Event {
        thread_id: i64,
        event: ThreadEvent,
    },
    Completed(Result<TurnCompletion>),
}

pub(crate) struct App {
    pub(crate) endpoint: PathBuf,
    pub(crate) history_path: PathBuf,
    pub(crate) threads: Vec<Thread>,
    pub(crate) models: Vec<Model>,
    pub(crate) thread_id: Option<i64>,
    pub(crate) transcript: Vec<TranscriptItem>,
    pub(crate) events: Vec<ThreadEvent>,
    pub(crate) tool_call_preview: Option<(String, usize)>,
    pub(crate) input: String,
    pub(crate) input_cursor: usize,
    pub(crate) input_history: Vec<String>,
    pub(crate) history_index: Option<usize>,
    pub(crate) history_draft: String,
    pub(crate) word_segmenter: WordSegmenterBorrowed<'static>,
    pub(crate) status: String,
    pub(crate) approval: Option<Approval>,
    pub(crate) renaming: bool,
    pub(crate) model_picker: Option<ModelPicker>,
    pub(crate) new_thread_model: Option<(String, String)>,
    pub(crate) login_required: bool,
    pub(crate) selection_start: Option<SelectionPoint>,
    pub(crate) selection_end: Option<SelectionPoint>,
    pub(crate) transcript_layout: TranscriptLayout,
    pub(crate) sidebar: Rect,
    pub(crate) turn_pending: bool,
    pub(crate) transcript_mode: TranscriptMode,
    pub(crate) focus: FocusPane,
    pub(crate) transcript_scroll: usize,
    pub(crate) transcript_horizontal_scroll: usize,
    pub(crate) detail_scroll: usize,
    pub(crate) selected_request: Option<usize>,
    pub(crate) raw_request: bool,
    pub(crate) expanded_tools: HashSet<usize>,
    pub(crate) selected_tool: Option<usize>,
    pub(crate) transcript_area: Rect,
    pub(crate) request_list_area: Rect,
    pub(crate) detail_area: Rect,
    pub(crate) tool_areas: Vec<(usize, Rect)>,
}

impl App {
    pub(super) async fn load(endpoint: PathBuf, history_path: PathBuf) -> Result<Self> {
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
        Ok(Self {
            endpoint,
            input_history: load_history(&history_path)?,
            history_path,
            threads,
            models,
            thread_id,
            transcript,
            events,
            tool_call_preview: None,
            input: String::new(),
            input_cursor: 0,
            history_index: None,
            history_draft: String::new(),
            word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
            status: if login_required {
                "Codex login required · Ctrl-L login".to_owned()
            } else {
                "Enter sends · Ctrl-T view · Tab focus · Ctrl-N new · Ctrl-M model · Ctrl-C copies"
                    .to_owned()
            },
            approval: None,
            renaming: false,
            model_picker: None,
            new_thread_model: None,
            login_required,
            selection_start: None,
            selection_end: None,
            transcript_layout: TranscriptLayout {
                text: String::new(),
                rows: Vec::new(),
            },
            sidebar: Rect::default(),
            turn_pending: false,
            transcript_mode: TranscriptMode::Coding,
            focus: FocusPane::Input,
            transcript_scroll: 0,
            transcript_horizontal_scroll: 0,
            detail_scroll: 0,
            selected_request: None,
            raw_request: false,
            expanded_tools: HashSet::new(),
            selected_tool: None,
            transcript_area: Rect::default(),
            request_list_area: Rect::default(),
            detail_area: Rect::default(),
            tool_areas: Vec::new(),
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
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('m') {
            self.open_model_picker()?;
            return Ok(false);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => return Ok(true),
                KeyCode::Char('t') => {
                    self.transcript_mode = match self.transcript_mode {
                        TranscriptMode::Coding => TranscriptMode::Debug,
                        TranscriptMode::Debug => TranscriptMode::Coding,
                    };
                    self.focus = FocusPane::Input;
                    self.clear_selection();
                    self.status = match self.transcript_mode {
                        TranscriptMode::Coding => "Coding transcript".to_owned(),
                        TranscriptMode::Debug => "LLM request inspector".to_owned(),
                    };
                }
                KeyCode::Char('n') => {
                    self.thread_id = None;
                    self.transcript.clear();
                    self.events.clear();
                    self.tool_call_preview = None;
                    self.input.clear();
                    self.input_cursor = 0;
                    self.reset_history_navigation();
                    self.approval = None;
                    self.renaming = false;
                    self.model_picker = None;
                    self.new_thread_model = None;
                    self.clear_selection();
                    self.reset_view();
                }
                KeyCode::Char('r') if self.thread_id.is_some() => {
                    self.renaming = true;
                    self.model_picker = None;
                    self.input = self
                        .threads
                        .iter()
                        .find(|thread| Some(thread.id) == self.thread_id)
                        .and_then(|thread| thread.display_name.clone())
                        .unwrap_or_default();
                    self.input_cursor = self.input.len();
                    self.reset_history_navigation();
                    self.status = "Enter saves the thread name · Esc cancels".to_owned();
                }
                KeyCode::Char('m') => {
                    self.open_model_picker()?;
                }
                KeyCode::Char('l') if self.login_required => {
                    self.status = "Complete Codex login in your browser…".to_owned();
                    match request(&self.endpoint, ControllerRequest::CodexLogin).await? {
                        ControllerResponse::CodexLoggedIn { .. } => {
                            self.login_required = false;
                            self.status = "Codex login complete".to_owned();
                        }
                        ControllerResponse::Error { message } => {
                            self.status = sanitize(&message);
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
                    if !self.input.is_empty() {
                        let input = std::mem::take(&mut self.input);
                        self.record_history(input)?;
                    }
                    self.input_cursor = 0;
                    self.reset_history_navigation();
                }
                KeyCode::Char('a') => self.input_cursor = 0,
                KeyCode::Char('e') => self.input_cursor = self.input.len(),
                KeyCode::Char('u') => {
                    self.input.drain(..self.input_cursor);
                    self.input_cursor = 0;
                    self.reset_history_navigation();
                }
                KeyCode::Char('k') => {
                    self.input.truncate(self.input_cursor);
                    self.reset_history_navigation();
                }
                KeyCode::Char('w') | KeyCode::Backspace => self.delete_word_backward(),
                KeyCode::Left => self.move_word_backward(),
                KeyCode::Right => self.move_word_forward(),
                _ => {}
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

        if let Some(approval) = &self.approval {
            match key.code {
                KeyCode::Char('y') => {
                    let id = approval.id;
                    self.resolve_approval(id, true, turns);
                }
                KeyCode::Char('n') => {
                    let id = approval.id;
                    self.resolve_approval(id, false, turns);
                }
                _ => {}
            }
            return Ok(false);
        }

        if self.renaming {
            match key.code {
                KeyCode::Enter if !self.input.trim().is_empty() => self.rename().await?,
                KeyCode::Backspace => self.delete_backward(),
                KeyCode::Delete => self.delete_forward(),
                KeyCode::Left => self.move_backward(),
                KeyCode::Right => self.move_forward(),
                KeyCode::Home => self.input_cursor = 0,
                KeyCode::End => self.input_cursor = self.input.len(),
                KeyCode::Char(character) => self.insert(character),
                KeyCode::Esc => {
                    self.renaming = false;
                    self.input.clear();
                    self.input_cursor = 0;
                    self.status = "Ready".to_owned();
                }
                _ => {}
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
                    self.status =
                        "Select reasoning effort · Enter applies · Esc goes back".to_owned();
                }
                KeyCode::Esc => {
                    if picker.selecting_effort {
                        picker.selecting_effort = false;
                        self.status =
                            "Select model · Enter chooses effort · Esc cancels".to_owned();
                    } else {
                        self.model_picker = None;
                        self.status = "Ready".to_owned();
                    }
                }
                _ => {}
            }
            return Ok(false);
        }

        if self.focus != FocusPane::Input {
            self.handle_pane_key(key);
            return Ok(false);
        }

        match key.code {
            KeyCode::Enter if !self.input.trim().is_empty() && !self.turn_pending => {
                self.send(turns)?
            }
            KeyCode::Backspace => self.delete_backward(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left => self.move_backward(),
            KeyCode::Right => self.move_forward(),
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::End => self.input_cursor = self.input.len(),
            KeyCode::Up => self.previous_history(),
            KeyCode::Down => self.next_history(),
            KeyCode::Char(character) => self.insert(character),
            KeyCode::Esc => self.clear_selection(),
            _ => {}
        }
        Ok(false)
    }

    fn open_model_picker(&mut self) -> Result<()> {
        self.renaming = false;
        if self.models.is_empty() {
            self.status = "No models are available".to_owned();
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
        self.status = "Select model · Enter chooses effort · Esc cancels".to_owned();
        Ok(())
    }

    async fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
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
            && self.sidebar.contains((mouse.column, mouse.row).into())
            && mouse.row > self.sidebar.y
        {
            let index = usize::from(mouse.row - self.sidebar.y - 1);
            if index == 0 {
                self.thread_id = None;
                self.transcript.clear();
                self.events.clear();
                self.tool_call_preview = None;
                self.approval = None;
                self.renaming = false;
                self.model_picker = None;
                self.new_thread_model = None;
                self.clear_selection();
                self.reset_view();
            } else if let Some(thread) = self.threads.get(index - 1) {
                self.thread_id = Some(thread.id);
                (self.transcript, self.events) = load_transcript(&self.endpoint, thread.id).await?;
                self.tool_call_preview = None;
                self.approval = None;
                self.renaming = false;
                self.model_picker = None;
                self.clear_selection();
                self.reset_view();
            }
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
                .tool_areas
                .iter()
                .find(|(_, area)| area.contains((mouse.column, mouse.row).into()))
        {
            let index = *index;
            self.selected_tool = Some(index);
            if !self.expanded_tools.remove(&index) {
                self.expanded_tools.insert(index);
            }
            self.focus = FocusPane::Transcript;
            return Ok(());
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
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

    fn send(&mut self, turns: &mpsc::UnboundedSender<TurnUpdate>) -> Result<()> {
        let message = std::mem::take(&mut self.input);
        self.input_cursor = 0;
        self.record_history(message.clone())?;
        self.reset_history_navigation();
        self.transcript
            .push(TranscriptItem::new(Role::User, sanitize(&message)));
        self.turn_pending = true;
        self.status = "Waiting for Atra Controller…".to_owned();
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

    fn record_history(&mut self, input: String) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.history_path)
            .with_context(|| {
                format!("failed to open TUI history {}", self.history_path.display())
            })?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to set TUI history permissions {}",
                    self.history_path.display()
                )
            })?;
        let mut line = serde_json::to_vec(&input).context("failed to encode TUI history")?;
        line.push(b'\n');
        file.write_all(&line).with_context(|| {
            format!(
                "failed to write TUI history {}",
                self.history_path.display()
            )
        })?;
        self.input_history.push(input);
        Ok(())
    }

    async fn rename(&mut self) -> Result<()> {
        let thread_id = self.thread_id.context("no thread is selected")?;
        let display_name = std::mem::take(&mut self.input);
        self.input_cursor = 0;
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
                self.status = "Thread renamed".to_owned();
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
            self.status = "Model selected for new thread".to_owned();
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
                self.status = "Thread model changed".to_owned();
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
        turns: &mpsc::UnboundedSender<TurnUpdate>,
    ) {
        let request_message = if allowed {
            ControllerRequest::ApprovalAllow { approval_id }
        } else {
            ControllerRequest::ApprovalDeny {
                approval_id,
                reason: None,
            }
        };
        self.approval = None;
        self.turn_pending = true;
        self.status = "Waiting for Atra Controller…".to_owned();
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
                        item.role == Role::User && item.text == sanitize(&message)
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
                        .is_some_and(|item| item.role == Role::Assistant)
                    {
                        self.transcript
                            .last_mut()
                            .unwrap()
                            .append(&sanitize(&content));
                    } else {
                        self.transcript
                            .push(TranscriptItem::new(Role::Assistant, sanitize(&content)));
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
                    self.transcript.push(TranscriptItem::new(
                        Role::Tool,
                        format!("{} ", sanitize(&name)),
                    ));
                    self.tool_call_preview = Some((item_id, index));
                }
                return Ok(());
            }
            TurnUpdate::ToolCallDelta {
                thread_id,
                item_id,
                delta,
            } => {
                if self.thread_id == Some(thread_id)
                    && let Some((preview_id, index)) = &self.tool_call_preview
                    && preview_id == &item_id
                {
                    self.transcript[*index].append(&sanitize(&delta));
                }
                return Ok(());
            }
            TurnUpdate::Event { thread_id, event } => {
                if self.thread_id == Some(thread_id) {
                    self.events.push(event.clone());
                    let item_id = event
                        .payload
                        .get("item_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    let Some(item) = event_to_item(event) else {
                        return Ok(());
                    };
                    if item.role == Role::Tool
                        && let Some(item_id) = item_id
                        && let Some((preview_id, index)) = self.tool_call_preview.take()
                        && preview_id == item_id
                    {
                        self.transcript[index] = item;
                        return Ok(());
                    }
                    self.transcript.push(item);
                }
                return Ok(());
            }
            TurnUpdate::Completed(Ok(completion)) => completion,
            TurnUpdate::Completed(Err(error)) => {
                if let Some((_, index)) = self.tool_call_preview.take() {
                    self.transcript.remove(index);
                }
                self.turn_pending = false;
                self.status = sanitize(&format!("{error:#}"));
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
                        .is_some_and(|item| item.role == Role::Assistant) =>
                {
                    self.status = "Ready".to_owned();
                }
                response => self.accept_turn_response(response)?,
            }
        } else {
            self.status = "Ready".to_owned();
        }
        Ok(())
    }

    fn accept_turn_response(&mut self, response: ControllerResponse) -> Result<()> {
        match response {
            ControllerResponse::TurnCompleted { content } => {
                self.transcript
                    .push(TranscriptItem::new(Role::Assistant, sanitize(&content)));
                self.status = "Ready".to_owned();
            }
            ControllerResponse::ApprovalRequired {
                approval_id,
                tool,
                arguments,
                ..
            } => {
                self.approval = Some(Approval {
                    id: approval_id,
                    description: sanitize(&format!("{tool} {arguments}")),
                });
                self.status = "Approval required: y allow · n deny".to_owned();
            }
            ControllerResponse::Error { message } => {
                self.status = sanitize(&message);
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
        let text = &self.transcript_layout.text[start..end];
        write!(
            io::stdout(),
            "\x1b]52;c;{}\x07",
            STANDARD.encode(text.as_bytes())
        )
        .context("failed to write OSC 52 clipboard sequence")?;
        io::stdout().flush().context("failed to flush OSC 52")?;
        self.status = "Copied selection".to_owned();
        Ok(())
    }

    fn clear_selection(&mut self) {
        self.selection_start = None;
        self.selection_end = None;
    }

    fn reset_view(&mut self) {
        self.transcript_scroll = 0;
        self.transcript_horizontal_scroll = 0;
        self.detail_scroll = 0;
        self.selected_request = None;
        self.raw_request = false;
        self.expanded_tools.clear();
        self.selected_tool = None;
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
                KeyCode::Left => {
                    self.transcript_horizontal_scroll =
                        self.transcript_horizontal_scroll.saturating_sub(4)
                }
                KeyCode::Right => {
                    self.transcript_horizontal_scroll =
                        self.transcript_horizontal_scroll.saturating_add(4)
                }
                KeyCode::Home => self.transcript_horizontal_scroll = 0,
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
                KeyCode::Up => self.select_tool(false),
                KeyCode::Down => self.select_tool(true),
                KeyCode::Enter => {
                    if let Some(index) = self.selected_tool
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

    fn select_tool(&mut self, forward: bool) {
        let tools = self
            .transcript
            .iter()
            .enumerate()
            .filter_map(|(index, item)| (item.role == Role::ToolResult).then_some(index))
            .collect::<Vec<_>>();
        if tools.is_empty() {
            self.selected_tool = None;
            return;
        }
        let current = self
            .selected_tool
            .and_then(|selected| tools.iter().position(|index| *index == selected));
        let next = match (current, forward) {
            (Some(index), true) => (index + 1).min(tools.len() - 1),
            (Some(index), false) => index.saturating_sub(1),
            (None, true) => 0,
            (None, false) => tools.len() - 1,
        };
        self.selected_tool = Some(tools[next]);
    }
}

pub(crate) fn layout_transcript(
    items: &[TranscriptItem],
    area: Rect,
    scroll: u16,
    horizontal_scroll: u16,
) -> TranscriptLayout {
    let mut text = String::new();
    let mut rows = Vec::new();
    let mut virtual_y = 0_u16;
    for item in items {
        virtual_y += 1;
        let item_start = text.len();
        text.push_str(&item.text);
        for source_line in item.text.split_inclusive('\n') {
            let content = source_line.strip_suffix('\n').unwrap_or(source_line);
            let content_start =
                item_start + source_line.as_ptr() as usize - item.text.as_ptr() as usize;
            let mut cells = Vec::new();
            for (byte, character) in content.char_indices() {
                let character_width = character.width().unwrap_or(0);
                cells.extend(std::iter::repeat_n(content_start + byte, character_width));
            }
            if virtual_y >= scroll && virtual_y - scroll < area.height {
                let start = usize::from(horizontal_scroll).min(cells.len());
                let end = (start + usize::from(area.width)).min(cells.len());
                rows.push(MappedRow {
                    x: area.x,
                    y: area.y + virtual_y - scroll,
                    cells: cells[start..end].to_vec(),
                    end: content_start + content.len(),
                });
            }
            virtual_y += 1;
        }
        text.push('\n');
    }
    TranscriptLayout { text, rows }
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

pub(crate) fn transcript_lines<'a>(
    items: &'a [TranscriptItem],
    selection: Option<(usize, usize)>,
    expanded_tools: &HashSet<usize>,
    selected_tool: Option<usize>,
) -> (Vec<Line<'a>>, Vec<(usize, std::ops::Range<usize>)>) {
    let mut lines = Vec::new();
    let mut tool_ranges = Vec::new();
    let mut offset = 0;
    for (item_index, item) in items.iter().enumerate() {
        let item_start = lines.len();
        lines.push(Line::from(Span::styled(
            item.role.label(),
            if selected_tool == Some(item_index) {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            },
        )));
        if matches!(item.role, Role::User | Role::Assistant) {
            lines.extend(item.markdown.as_ref().unwrap().iter().map(|line| {
                Line {
                    style: line.style,
                    alignment: line.alignment,
                    spans: line
                        .spans
                        .iter()
                        .map(|span| Span::styled(span.content.as_ref(), span.style))
                        .collect(),
                }
            }));
            offset += item.text.len() + 1;
            if item.role == Role::ToolResult {
                tool_ranges.push((item_index, item_start..lines.len()));
            }
            continue;
        }
        if item.role == Role::Tool && item.text.starts_with("apply_patch ") {
            lines.extend(patch_lines(
                item.text.strip_prefix("apply_patch ").unwrap_or(&item.text),
            ));
            lines.push(Line::default());
            offset += item.text.len() + 1;
            continue;
        }
        let display = if item.role == Role::ToolResult && !expanded_tools.contains(&item_index) {
            summarize_result(&item.text)
        } else {
            item.text.clone()
        };
        for source_line in display.lines() {
            let mut current = Vec::new();
            for (byte, character) in source_line.char_indices() {
                let selected = selection.is_some_and(|(start, end)| {
                    let absolute = offset + byte;
                    absolute >= start && absolute < end
                });
                let style = if selected {
                    Style::default().bg(Color::Blue)
                } else {
                    Style::default()
                };
                current.push(Span::styled(character.to_string(), style));
            }
            lines.push(Line::from(current));
            offset += source_line.len() + 1;
        }
        if !item.text.ends_with('\n') {
            offset = offset.saturating_sub(1);
        }
        offset += 1;
        if item.role == Role::ToolResult {
            tool_ranges.push((item_index, item_start..lines.len()));
        }
    }
    (lines, tool_ranges)
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
) -> Result<(Vec<TranscriptItem>, Vec<ThreadEvent>)> {
    match request(endpoint, ControllerRequest::ThreadEvents { thread_id }).await? {
        ControllerResponse::ThreadEvents { events } => Ok((
            events.iter().cloned().filter_map(event_to_item).collect(),
            events,
        )),
        ControllerResponse::Error { message } => bail!("{message}"),
        response => bail!("controller returned an unexpected response: {response:?}"),
    }
}

fn event_to_item(event: ThreadEvent) -> Option<TranscriptItem> {
    let (role, text) = match event.kind.as_str() {
        "user_message" => (
            Role::User,
            event.payload.get("content")?.as_str()?.to_owned(),
        ),
        "assistant_message" => (
            Role::Assistant,
            event.payload.get("content")?.as_str()?.to_owned(),
        ),
        "tool_call" => (
            Role::Tool,
            match event
                .payload
                .get("input")
                .and_then(serde_json::Value::as_str)
            {
                Some(input) => format!("{} {}", event.payload.get("name")?.as_str()?, input),
                None => format!(
                    "{} {}",
                    event.payload.get("name")?.as_str()?,
                    event.payload.get("arguments")?
                ),
            },
        ),
        "tool_result" => (
            Role::ToolResult,
            match event.payload.get("result")? {
                serde_json::Value::String(result) => result.clone(),
                result => serde_json::to_string_pretty(result).ok()?,
            },
        ),
        "approval_request" => (
            Role::ApprovalRequest,
            format!(
                "{} {}",
                event.payload.get("tool")?.as_str()?,
                event.payload.get("arguments")?
            ),
        ),
        "approval_response" => (
            Role::ApprovalResponse,
            event.payload.get("decision")?.as_str()?.to_owned(),
        ),
        _ => return None,
    };
    Some(TranscriptItem::new(role, sanitize(&text)))
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
