use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_protocol::{ControllerRequest, ControllerResponse, Model, Thread, ThreadEvent};
use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use icu_segmenter::{WordSegmenter, WordSegmenterBorrowed, options::WordBreakInvariantOptions};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub async fn run(endpoint: PathBuf, history_path: PathBuf) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let result = App::load(endpoint, history_path)
        .await?
        .run(&mut terminal.terminal)
        .await;
    terminal.restore()?;
    result
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    restored: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .context("failed to enter the terminal UI")?;
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stdout))
                .context("failed to initialize the terminal")?,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<()> {
        if !self.restored {
            disable_raw_mode().context("failed to disable terminal raw mode")?;
            execute!(
                self.terminal.backend_mut(),
                PopKeyboardEnhancementFlags,
                DisableMouseCapture,
                LeaveAlternateScreen
            )
            .context("failed to leave the terminal UI")?;
            self.terminal
                .show_cursor()
                .context("failed to show cursor")?;
            self.restored = true;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Clone)]
struct TranscriptItem {
    role: Role,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
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

struct Approval {
    id: u64,
    description: String,
}

struct ModelPicker {
    models: Vec<Model>,
    model_index: usize,
    effort_index: usize,
    selecting_effort: bool,
}

#[derive(Clone, Copy)]
struct SelectionPoint {
    offset: usize,
}

struct MappedRow {
    x: u16,
    y: u16,
    cells: Vec<usize>,
    end: usize,
}

struct TranscriptLayout {
    text: String,
    rows: Vec<MappedRow>,
}

struct TurnCompletion {
    thread_id: i64,
    response: ControllerResponse,
}

enum TurnUpdate {
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

struct App {
    endpoint: PathBuf,
    history_path: PathBuf,
    threads: Vec<Thread>,
    models: Vec<Model>,
    thread_id: Option<i64>,
    transcript: Vec<TranscriptItem>,
    tool_call_preview: Option<(String, usize)>,
    input: String,
    input_cursor: usize,
    input_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    word_segmenter: WordSegmenterBorrowed<'static>,
    status: String,
    approval: Option<Approval>,
    renaming: bool,
    model_picker: Option<ModelPicker>,
    new_thread_model: Option<(String, String)>,
    login_required: bool,
    selection_start: Option<SelectionPoint>,
    selection_end: Option<SelectionPoint>,
    transcript_layout: TranscriptLayout,
    sidebar: Rect,
    turn_pending: bool,
}

impl App {
    async fn load(endpoint: PathBuf, history_path: PathBuf) -> Result<Self> {
        let threads = match request(&endpoint, ControllerRequest::ThreadList).await? {
            ControllerResponse::ThreadList { threads } => threads,
            ControllerResponse::Error { message } => bail!("{message}"),
            response => bail!("controller returned an unexpected response: {response:?}"),
        };
        let thread_id = threads.first().map(|thread| thread.id);
        let transcript = match thread_id {
            Some(thread_id) => load_transcript(&endpoint, thread_id).await?,
            None => Vec::new(),
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
            tool_call_preview: None,
            input: String::new(),
            input_cursor: 0,
            history_index: None,
            history_draft: String::new(),
            word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
            status: if login_required {
                "Codex login required · Ctrl-L login".to_owned()
            } else {
                "Enter sends · Ctrl-N new · Ctrl-R rename · Ctrl-M/Alt-M model · Ctrl-C copies"
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
        })
    }

    async fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
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
                KeyCode::Char('n') => {
                    self.thread_id = None;
                    self.transcript.clear();
                    self.tool_call_preview = None;
                    self.input.clear();
                    self.input_cursor = 0;
                    self.reset_history_navigation();
                    self.approval = None;
                    self.renaming = false;
                    self.model_picker = None;
                    self.new_thread_model = None;
                    self.clear_selection();
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

    fn insert(&mut self, character: char) {
        self.input.insert(self.input_cursor, character);
        self.input_cursor += character.len_utf8();
        self.reset_history_navigation();
    }

    fn delete_backward(&mut self) {
        if let Some((index, _)) = self.input[..self.input_cursor].char_indices().next_back() {
            self.input.drain(index..self.input_cursor);
            self.input_cursor = index;
            self.reset_history_navigation();
        }
    }

    fn delete_forward(&mut self) {
        if let Some(character) = self.input[self.input_cursor..].chars().next() {
            self.input
                .drain(self.input_cursor..self.input_cursor + character.len_utf8());
            self.reset_history_navigation();
        }
    }

    fn move_backward(&mut self) {
        if let Some((index, _)) = self.input[..self.input_cursor].char_indices().next_back() {
            self.input_cursor = index;
        }
    }

    fn move_forward(&mut self) {
        if let Some(character) = self.input[self.input_cursor..].chars().next() {
            self.input_cursor += character.len_utf8();
        }
    }

    fn delete_word_backward(&mut self) {
        let end = self.input_cursor;
        self.move_word_backward();
        if self.input_cursor < end {
            self.input.drain(self.input_cursor..end);
            self.reset_history_navigation();
        }
    }

    fn move_word_backward(&mut self) {
        let mut start = 0;
        let mut previous_word = None;
        for (end, word_type) in self
            .word_segmenter
            .segment_str(&self.input)
            .iter_with_word_type()
        {
            if word_type.is_word_like() && start < self.input_cursor {
                previous_word = Some(start);
            }
            if end >= self.input_cursor {
                break;
            }
            start = end;
        }
        self.input_cursor = previous_word.unwrap_or(0);
    }

    fn move_word_forward(&mut self) {
        for (end, word_type) in self
            .word_segmenter
            .segment_str(&self.input)
            .iter_with_word_type()
        {
            if word_type.is_word_like() && end > self.input_cursor {
                self.input_cursor = end;
                return;
            }
        }
        self.input_cursor = self.input.len();
    }

    fn previous_history(&mut self) {
        let index = match self.history_index {
            Some(0) => return,
            Some(index) => index - 1,
            None if self.input_history.is_empty() => return,
            None => {
                self.history_draft.clone_from(&self.input);
                self.input_history.len() - 1
            }
        };
        self.history_index = Some(index);
        self.input.clone_from(&self.input_history[index]);
        self.input_cursor = self.input.len();
    }

    fn next_history(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.input_history.len() {
            self.history_index = Some(index + 1);
            self.input.clone_from(&self.input_history[index + 1]);
        } else {
            self.history_index = None;
            self.input = std::mem::take(&mut self.history_draft);
        }
        self.input_cursor = self.input.len();
    }

    fn reset_history_navigation(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
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
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.sidebar.contains((mouse.column, mouse.row).into())
            && mouse.row > self.sidebar.y
        {
            let index = usize::from(mouse.row - self.sidebar.y - 1);
            if index == 0 {
                self.thread_id = None;
                self.transcript.clear();
                self.tool_call_preview = None;
                self.approval = None;
                self.renaming = false;
                self.model_picker = None;
                self.new_thread_model = None;
                self.clear_selection();
            } else if let Some(thread) = self.threads.get(index - 1) {
                self.thread_id = Some(thread.id);
                self.transcript = load_transcript(&self.endpoint, thread.id).await?;
                self.tool_call_preview = None;
                self.approval = None;
                self.renaming = false;
                self.model_picker = None;
                self.clear_selection();
            }
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
        self.transcript.push(TranscriptItem {
            role: Role::User,
            text: sanitize(&message),
        });
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
                            .text
                            .push_str(&sanitize(&content));
                    } else {
                        self.transcript.push(TranscriptItem {
                            role: Role::Assistant,
                            text: sanitize(&content),
                        });
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
                    self.transcript.push(TranscriptItem {
                        role: Role::Tool,
                        text: format!("{} ", sanitize(&name)),
                    });
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
                    self.transcript[*index].text.push_str(&sanitize(&delta));
                }
                return Ok(());
            }
            TurnUpdate::Event { thread_id, event } => {
                if self.thread_id == Some(thread_id) {
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
                self.transcript.push(TranscriptItem {
                    role: Role::Assistant,
                    text: sanitize(&content),
                });
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

    fn render(&mut self, frame: &mut Frame<'_>) {
        let [main, input, status] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(4),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(frame.area());
        let [sidebar, transcript] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(18), Constraint::Min(20)])
            .areas(main);
        self.sidebar = sidebar;

        let threads = std::iter::once(ListItem::new("+ New thread"))
            .chain(self.threads.iter().map(|thread| {
                let marker = if Some(thread.id) == self.thread_id {
                    "●"
                } else {
                    " "
                };
                let display_name = thread
                    .display_name
                    .as_deref()
                    .map(|name| sanitize(name).replace(['\n', '\t'], " "))
                    .unwrap_or_else(|| "Untitled thread".to_owned());
                ListItem::new(format!("{marker} {}", display_name))
            }))
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(threads).block(Block::default().title("Threads").borders(Borders::ALL)),
            sidebar,
        );

        let inner = Block::default()
            .title("Transcript")
            .borders(Borders::ALL)
            .inner(transcript);
        let lines = transcript_lines(&self.transcript, inner.width, self.selection_range());
        let scroll = lines.len().saturating_sub(usize::from(inner.height)) as u16;
        self.transcript_layout = layout_transcript(&self.transcript, inner, scroll);
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((scroll, 0))
                .block(Block::default().title("Transcript").borders(Borders::ALL)),
            transcript,
        );

        let input_title = if self.renaming {
            "Thread name".to_owned()
        } else {
            match &self.approval {
                Some(approval) => format!("Approval: {}", approval.description),
                None => "Message".to_owned(),
            }
        };
        let cursor_column = self.input[..self.input_cursor].width();
        let visible_input_width = usize::from(input.width.saturating_sub(2));
        let horizontal_scroll =
            cursor_column.saturating_sub(visible_input_width.saturating_sub(1)) as u16;
        frame.render_widget(
            Paragraph::new(self.input.as_str())
                .scroll((0, horizontal_scroll))
                .block(Block::default().title(input_title).borders(Borders::ALL)),
            input,
        );
        frame.render_widget(Paragraph::new(self.status.as_str()), status);
        if self.approval.is_none() && self.model_picker.is_none() {
            frame.set_cursor_position((
                input.x + 1 + cursor_column as u16 - horizontal_scroll,
                input.y + 1,
            ));
        }
        if let Some(picker) = &self.model_picker {
            render_model_picker(frame, picker);
        }
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

    fn selection_range(&self) -> Option<(usize, usize)> {
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
}

fn render_model_picker(frame: &mut Frame<'_>, picker: &ModelPicker) {
    let width = frame.area().width.saturating_sub(8).min(72);
    let height = frame.area().height.saturating_sub(4).min(18);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + (frame.area().height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let selected_model = &picker.models[picker.model_index];
    if picker.selecting_effort {
        let items = selected_model
            .supported_reasoning_efforts
            .iter()
            .enumerate()
            .map(|(index, effort)| {
                let marker = if index == picker.effort_index {
                    "●"
                } else {
                    " "
                };
                ListItem::new(format!("{marker} {effort}"))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(items).block(
                Block::default()
                    .title(format!(
                        "Reasoning effort · {}",
                        selected_model.display_name
                    ))
                    .borders(Borders::ALL),
            ),
            area,
        );
    } else {
        let items = picker
            .models
            .iter()
            .enumerate()
            .map(|(index, model)| {
                let marker = if index == picker.model_index {
                    "●"
                } else {
                    " "
                };
                let description = model.description.as_deref().unwrap_or_default();
                ListItem::new(vec![
                    Line::from(format!("{marker} {}", model.display_name)),
                    Line::from(Span::styled(
                        format!("  {description}"),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(items).block(Block::default().title("Select model").borders(Borders::ALL)),
            area,
        );
    }
}

fn layout_transcript(items: &[TranscriptItem], area: Rect, scroll: u16) -> TranscriptLayout {
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
            let mut width = 0;
            let mut cells = Vec::new();
            for (byte, character) in content.char_indices() {
                let character_width = character.width().unwrap_or(0);
                if width + character_width > usize::from(area.width) && !cells.is_empty() {
                    if virtual_y >= scroll && virtual_y - scroll < area.height {
                        rows.push(MappedRow {
                            x: area.x,
                            y: area.y + virtual_y - scroll,
                            cells,
                            end: content_start + byte,
                        });
                    }
                    cells = Vec::new();
                    width = 0;
                    virtual_y += 1;
                }
                cells.extend(std::iter::repeat_n(content_start + byte, character_width));
                width += character_width;
            }
            if virtual_y >= scroll && virtual_y - scroll < area.height {
                rows.push(MappedRow {
                    x: area.x,
                    y: area.y + virtual_y - scroll,
                    cells,
                    end: content_start + content.len(),
                });
            }
            virtual_y += 1;
        }
        text.push('\n');
    }
    TranscriptLayout { text, rows }
}

fn transcript_lines<'a>(
    items: &'a [TranscriptItem],
    width: u16,
    selection: Option<(usize, usize)>,
) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for item in items {
        lines.push(Line::from(Span::styled(
            item.role.label(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for source_line in item.text.lines() {
            let mut current = Vec::new();
            let mut current_width = 0;
            for (byte, character) in source_line.char_indices() {
                let character_width = character.width().unwrap_or(0);
                if current_width + character_width > usize::from(width) && !current.is_empty() {
                    lines.push(Line::from(current));
                    current = Vec::new();
                    current_width = 0;
                }
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
                current_width += character_width;
            }
            lines.push(Line::from(current));
            offset += source_line.len() + 1;
        }
        if !item.text.ends_with('\n') {
            offset = offset.saturating_sub(1);
        }
        offset += 1;
    }
    lines
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

async fn load_transcript(endpoint: &Path, thread_id: i64) -> Result<Vec<TranscriptItem>> {
    match request(endpoint, ControllerRequest::ThreadEvents { thread_id }).await? {
        ControllerResponse::ThreadEvents { events } => Ok(events
            .into_iter()
            .filter_map(event_to_item)
            .collect::<Vec<_>>()),
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
    Some(TranscriptItem {
        role,
        text: sanitize(&text),
    })
}

fn sanitize(input: &str) -> String {
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

async fn request(endpoint: &Path, request: ControllerRequest) -> Result<ControllerResponse> {
    let mut stream = UnixStream::connect(endpoint)
        .await
        .with_context(|| format!("failed to connect to controller at {}", endpoint.display()))?;
    let mut encoded =
        serde_json::to_vec(&request).context("failed to encode controller request")?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .await
        .context("failed to write controller request")?;
    let mut response = String::new();
    tokio::time::timeout(
        Duration::from_secs(300),
        BufReader::new(stream).read_line(&mut response),
    )
    .await
    .context("controller request timed out")?
    .context("failed to read controller response")?;
    serde_json::from_str(&response).context("failed to decode controller response")
}

async fn request_stream(
    endpoint: &Path,
    request: ControllerRequest,
    thread_id: i64,
    updates: &mpsc::UnboundedSender<TurnUpdate>,
) -> Result<ControllerResponse> {
    let mut stream = UnixStream::connect(endpoint)
        .await
        .with_context(|| format!("failed to connect to controller at {}", endpoint.display()))?;
    let mut encoded =
        serde_json::to_vec(&request).context("failed to encode controller request")?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .await
        .context("failed to write controller request")?;
    let mut responses = BufReader::new(stream).lines();
    loop {
        let response = tokio::time::timeout(Duration::from_secs(300), responses.next_line())
            .await
            .context("controller request timed out")?
            .context("failed to read controller response")?
            .context("controller closed the response stream")?;
        match serde_json::from_str(&response).context("failed to decode controller response")? {
            ControllerResponse::TurnDelta { content } => {
                updates.send(TurnUpdate::Delta { thread_id, content }).ok();
            }
            ControllerResponse::ToolCallStarted { item_id, name } => {
                updates
                    .send(TurnUpdate::ToolCallStarted {
                        thread_id,
                        item_id,
                        name,
                    })
                    .ok();
            }
            ControllerResponse::ToolCallDelta { item_id, delta } => {
                updates
                    .send(TurnUpdate::ToolCallDelta {
                        thread_id,
                        item_id,
                        delta,
                    })
                    .ok();
            }
            ControllerResponse::TurnEvent { event } => {
                updates.send(TurnUpdate::Event { thread_id, event }).ok();
            }
            response => return Ok(response),
        }
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
