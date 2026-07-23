use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_protocol::{ControllerRequest, ControllerResponse, Thread, ThreadEvent};
use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use unicode_width::UnicodeWidthChar;

pub async fn run(endpoint: PathBuf) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let result = App::load(endpoint).await?.run(&mut terminal.terminal).await;
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
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
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
    role: &'static str,
    text: String,
}

struct Approval {
    id: u64,
    description: String,
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

struct App {
    endpoint: PathBuf,
    threads: Vec<Thread>,
    thread_id: Option<i64>,
    transcript: Vec<TranscriptItem>,
    input: String,
    status: String,
    approval: Option<Approval>,
    renaming: bool,
    selection_start: Option<SelectionPoint>,
    selection_end: Option<SelectionPoint>,
    transcript_layout: TranscriptLayout,
    sidebar: Rect,
}

impl App {
    async fn load(endpoint: PathBuf) -> Result<Self> {
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
        Ok(Self {
            endpoint,
            threads,
            thread_id,
            transcript,
            input: String::new(),
            status: "Enter sends · Ctrl-N new · Ctrl-R rename · drag to select · Ctrl-C copies"
                .to_owned(),
            approval: None,
            renaming: false,
            selection_start: None,
            selection_end: None,
            transcript_layout: TranscriptLayout {
                text: String::new(),
                rows: Vec::new(),
            },
            sidebar: Rect::default(),
        })
    }

    async fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        let mut events = EventStream::new();
        loop {
            terminal.draw(|frame| self.render(frame))?;
            let Some(event) = events.next().await.transpose()? else {
                return Ok(());
            };
            match event {
                Event::Key(key) if self.handle_key(key).await? => return Ok(()),
                Event::Mouse(mouse) => self.handle_mouse(mouse).await?,
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => return Ok(true),
                KeyCode::Char('n') => {
                    self.thread_id = None;
                    self.transcript.clear();
                    self.input.clear();
                    self.approval = None;
                    self.renaming = false;
                    self.clear_selection();
                }
                KeyCode::Char('r') if self.thread_id.is_some() => {
                    self.renaming = true;
                    self.input = self
                        .threads
                        .iter()
                        .find(|thread| Some(thread.id) == self.thread_id)
                        .and_then(|thread| thread.display_name.clone())
                        .unwrap_or_default();
                    self.status = "Enter saves the thread name · Esc cancels".to_owned();
                }
                KeyCode::Char('c') => self.copy_selection()?,
                _ => {}
            }
            return Ok(false);
        }

        if let Some(approval) = &self.approval {
            match key.code {
                KeyCode::Char('y') => {
                    let id = approval.id;
                    self.resolve_approval(id, true).await?;
                }
                KeyCode::Char('n') => {
                    let id = approval.id;
                    self.resolve_approval(id, false).await?;
                }
                _ => {}
            }
            return Ok(false);
        }

        if self.renaming {
            match key.code {
                KeyCode::Enter if !self.input.trim().is_empty() => self.rename().await?,
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(character) => self.input.push(character),
                KeyCode::Esc => {
                    self.renaming = false;
                    self.input.clear();
                    self.status = "Ready".to_owned();
                }
                _ => {}
            }
            return Ok(false);
        }

        match key.code {
            KeyCode::Enter if !self.input.trim().is_empty() => self.send().await?,
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(character) => self.input.push(character),
            KeyCode::Esc => self.clear_selection(),
            _ => {}
        }
        Ok(false)
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
                self.approval = None;
                self.renaming = false;
                self.clear_selection();
            } else if let Some(thread) = self.threads.get(index - 1) {
                self.thread_id = Some(thread.id);
                self.transcript = load_transcript(&self.endpoint, thread.id).await?;
                self.approval = None;
                self.renaming = false;
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

    async fn send(&mut self) -> Result<()> {
        let message = std::mem::take(&mut self.input);
        let thread_id = match self.thread_id {
            Some(thread_id) => thread_id,
            None => match request(
                &self.endpoint,
                ControllerRequest::ThreadCreate { display_name: None },
            )
            .await?
            {
                ControllerResponse::ThreadCreated { thread_id } => {
                    self.threads.insert(
                        0,
                        Thread {
                            id: thread_id,
                            display_name: None,
                        },
                    );
                    self.thread_id = Some(thread_id);
                    thread_id
                }
                ControllerResponse::Error { message } => bail!("{message}"),
                response => bail!("controller returned an unexpected response: {response:?}"),
            },
        };
        self.transcript.push(TranscriptItem {
            role: "You",
            text: sanitize(&message),
        });
        if let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
            && thread.display_name.is_none()
        {
            thread.display_name = Some(message.clone());
        }
        self.status = "Waiting for Atra Controller…".to_owned();
        let response = request(
            &self.endpoint,
            ControllerRequest::ThreadSend { thread_id, message },
        )
        .await?;
        self.accept_turn_response(response)
    }

    async fn rename(&mut self) -> Result<()> {
        let thread_id = self.thread_id.context("no thread is selected")?;
        let display_name = std::mem::take(&mut self.input);
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

    async fn resolve_approval(&mut self, approval_id: u64, allowed: bool) -> Result<()> {
        let request_message = if allowed {
            ControllerRequest::ApprovalAllow { approval_id }
        } else {
            ControllerRequest::ApprovalDeny {
                approval_id,
                reason: None,
            }
        };
        self.approval = None;
        self.status = "Waiting for Atra Controller…".to_owned();
        let response = request(&self.endpoint, request_message).await?;
        self.accept_turn_response(response)
    }

    fn accept_turn_response(&mut self, response: ControllerResponse) -> Result<()> {
        match response {
            ControllerResponse::TurnCompleted { content } => {
                self.transcript.push(TranscriptItem {
                    role: "Atra",
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
        frame.render_widget(
            Paragraph::new(self.input.as_str())
                .block(Block::default().title(input_title).borders(Borders::ALL)),
            input,
        );
        frame.render_widget(Paragraph::new(self.status.as_str()), status);
        if self.approval.is_none() {
            frame.set_cursor_position((input.x + 1 + self.input.len() as u16, input.y + 1));
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
            item.role,
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
        "user_message" => ("You", event.payload.get("content")?.as_str()?.to_owned()),
        "assistant_message" => ("Atra", event.payload.get("content")?.as_str()?.to_owned()),
        "tool_call" => (
            "Tool",
            format!(
                "{} {}",
                event.payload.get("name")?.as_str()?,
                event.payload.get("arguments")?
            ),
        ),
        "tool_result" => (
            "Tool result",
            serde_json::to_string_pretty(event.payload.get("result")?).ok()?,
        ),
        "approval_request" => (
            "Approval requested",
            format!(
                "{} {}",
                event.payload.get("tool")?.as_str()?,
                event.payload.get("arguments")?
            ),
        ),
        "approval_response" => (
            "Approval",
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

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
