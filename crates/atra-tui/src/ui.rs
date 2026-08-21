use atra_protocol::{ModelRequestKind, ProcessStatus, ThreadEventData, TurnPhase};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{App, COMMAND_HELP},
    state::{
        CheckpointPicker, FocusPane, ModelPicker, ModelPickerStage, OperationOverlay, Overlay,
        ProcessPicker, ProcessPickerState, QuestionForm, QuestionFormMode, ThreadPicker,
        ThreadPickerState, TurnState,
    },
    text::{expand_line_tabs, expand_tabs},
    transcript::{
        layout_transcript, prepare_transcript, sanitize, transcript_lines, transcript_ranges,
    },
};

fn quota_delta(first: &Value, current: &Value) -> Option<f64> {
    (first["resets_at"] == current["resets_at"]).then(|| {
        current["used_percent"].as_f64().unwrap_or_default()
            - first["used_percent"].as_f64().unwrap_or_default()
    })
}

fn format_quota_window(window: &Value, delta: Option<f64>) -> (String, Option<(String, f64)>) {
    let label = integer_value(&window["window_minutes"]).map_or_else(
        || "?".to_owned(),
        |minutes| {
            if minutes == 7 * 24 * 60 {
                "weekly".to_owned()
            } else {
                format_window_duration(minutes)
            }
        },
    );
    let remaining = (100.0 - window["used_percent"].as_f64().unwrap_or_default()).clamp(0.0, 100.0);
    let reset = integer_value(&window["resets_at"])
        .and_then(|reset| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
            Some(format_window_duration((reset - now).max(0) / 60))
        })
        .unwrap_or_else(|| "?".to_owned());
    let content = format!("{label} {remaining:.3}%/{reset}");
    let delta = delta
        .filter(|delta| *delta > 0.0)
        .map(|delta| (label, delta));
    (content, delta)
}

fn integer_value(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        let value = value.as_f64()?;
        (value.is_finite() && value >= i64::MIN as f64 && value <= i64::MAX as f64)
            .then_some(value as i64)
    })
}

fn has_quota_window(window: &Value) -> bool {
    window.get("used_percent").and_then(Value::as_f64).is_some()
}

fn format_window_duration(minutes: i64) -> String {
    if minutes >= 24 * 60 {
        format!("{:.0}d", minutes as f64 / (24 * 60) as f64)
    } else if minutes >= 60 {
        format!("{:.0}h", minutes as f64 / 60.0)
    } else {
        format!("{minutes}m")
    }
}

fn selected_input_text(value: &str, selection: Option<(usize, usize)>) -> Text<'static> {
    let selection_style = Style::default().bg(Color::DarkGray);
    let mut offset = 0;
    let lines = value
        .split('\n')
        .map(|line| {
            let spans = line
                .char_indices()
                .map(|(byte, character)| {
                    let selected = selection
                        .is_some_and(|(start, end)| start <= offset + byte && offset + byte < end);
                    Span::styled(
                        character.to_string(),
                        if selected {
                            selection_style
                        } else {
                            Style::default()
                        },
                    )
                })
                .collect::<Vec<_>>();
            offset += line.len() + 1;
            expand_line_tabs(Line::from(spans))
        })
        .collect::<Vec<_>>();
    Text::from(lines)
}

pub(crate) fn preserve_transcript_viewport(
    current_scroll: usize,
    previous_max_scroll: usize,
    max_scroll: usize,
) -> usize {
    if current_scroll == 0 {
        return 0;
    }

    let viewport_start =
        previous_max_scroll.saturating_sub(current_scroll.min(previous_max_scroll));
    max_scroll.saturating_sub(viewport_start.min(max_scroll))
}

impl App {
    pub(super) fn render(&mut self, frame: &mut Frame<'_>) {
        let input_height = if self.pending_approval().is_some()
            || matches!(self.turn, TurnState::EnteringDenyReason { .. })
        {
            3
        } else if let TurnState::AnsweringQuestions(form) = &self.turn {
            (form.drafts[form.current]
                .note
                .value
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as u16
                + 3)
            .min(8)
        } else {
            (self
                .message_input
                .value
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as u16
                + 3)
            .min(frame.area().height.saturating_sub(6).max(3))
        };
        let [main, input, status] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(4),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .areas(frame.area());
        self.layout.input_area = input;
        let (transcript_main, question_area) =
            if let TurnState::AnsweringQuestions(form) = &self.turn {
                let question_height = question_form_height(form, main.height.saturating_sub(4));
                let [transcript, question] = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(4), Constraint::Length(question_height)])
                    .areas(main);
                (transcript, Some(question))
            } else {
                (main, None)
            };
        let transcript_area = if self.target.checkpoint_picker().is_some() {
            let [_, transcript] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(30), Constraint::Min(20)])
                .areas(transcript_main);
            transcript
        } else {
            transcript_main
        };
        self.layout.transcript_area = transcript_area;
        self.render_coding_transcript(frame, transcript_area);
        if let (Some(area), TurnState::AnsweringQuestions(form)) = (question_area, &self.turn) {
            render_question_form(frame, area, form);
        }
        self.render_composer(frame, input);
        frame.render_widget(Paragraph::new(self.status_line()), status);
        if let Some(breadcrumb) = self.thread_breadcrumb() {
            let width = breadcrumb.width().min(usize::from(status.width)) as u16;
            let area = Rect {
                x: status.right().saturating_sub(width),
                width,
                ..status
            };
            frame.render_widget(Paragraph::new(breadcrumb), area);
        }
        self.render_overlays(frame, main);
        if let Some(error) = &self.error {
            render_error(frame, error);
        }
    }

    fn render_composer(&mut self, frame: &mut Frame<'_>, input: Rect) {
        let (input_title, input_hint, input_value, input_cursor, input_selection, show_cursor) =
            if let TurnState::EnteringDenyReason { reason, .. } = &self.turn {
                (
                    "Deny reason (optional)".to_owned(),
                    Some(Line::from("Enter: deny · Esc: back").right_aligned()),
                    reason.value.as_str(),
                    reason.cursor,
                    reason.selection_range(),
                    true,
                )
            } else if let Some(approval) = self.pending_approval() {
                let operation = approval
                    .operation_index()
                    .map(|index| format!("Operation {index} · "))
                    .unwrap_or_default();
                let runner = approval
                    .arguments()
                    .get("runner")
                    .and_then(serde_json::Value::as_str)
                    .filter(|runner| !runner.is_empty())
                    .map(|runner| format!("{runner} · "))
                    .unwrap_or_default();
                let label = approval
                    .operation_label()
                    .unwrap_or_else(|| approval.tool());
                (
                    format!("Approval required · {operation}{runner}{label}"),
                    None,
                    "[y] Allow  [n] Deny",
                    0,
                    None,
                    false,
                )
            } else if let TurnState::AnsweringQuestions(form) = &self.turn {
                let note = &form.drafts[form.current].note;
                (
                    format!(
                        "Note (optional) · {}/{}",
                        form.current + 1,
                        form.request.questions.len()
                    ),
                    (form.mode == QuestionFormMode::Note).then(|| {
                        Line::from(
                            "Enter: newline · Ctrl-Enter / Ctrl-G: next · Tab / Esc: options",
                        )
                        .right_aligned()
                    }),
                    note.value.as_str(),
                    note.cursor,
                    (form.mode == QuestionFormMode::Note)
                        .then(|| note.selection_range())
                        .flatten(),
                    form.mode == QuestionFormMode::Note,
                )
            } else {
                match &self.overlay {
                    Overlay::Rename => (
                        "Thread name".to_owned(),
                        None,
                        self.message_input.value.as_str(),
                        self.message_input.cursor,
                        self.message_input.selection_range(),
                        true,
                    ),
                    _ => (
                        "Message".to_owned(),
                        Some(Line::from("Enter: newline · Ctrl-G: send").right_aligned()),
                        self.message_input.value.as_str(),
                        self.message_input.cursor,
                        self.message_input.selection_range(),
                        true,
                    ),
                }
            };
        let mut input_block = Block::default().title(input_title);
        if let Some(input_hint) = input_hint {
            input_block = input_block.title_bottom(input_hint);
        }
        let input_before_cursor = expand_tabs(&input_value[..input_cursor]);
        let cursor_row = input_before_cursor
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let cursor_column = input_before_cursor
            .rsplit_once('\n')
            .map_or(input_before_cursor.as_str(), |(_, line)| line)
            .width();
        let visible_input_width = usize::from(input.width.saturating_sub(2));
        let visible_input_height = usize::from(input.height.saturating_sub(2));
        let horizontal_scroll =
            cursor_column.saturating_sub(visible_input_width.saturating_sub(1)) as u16;
        let vertical_scroll =
            cursor_row.saturating_sub(visible_input_height.saturating_sub(1)) as u16;
        self.layout.input_text_area = Rect::new(
            input.x.saturating_add(1),
            input.y.saturating_add(1),
            input.width.saturating_sub(2),
            input.height.saturating_sub(2),
        );
        self.layout.input_scroll = (vertical_scroll, horizontal_scroll);
        frame.render_widget(
            Paragraph::new(selected_input_text(input_value, input_selection))
                .scroll((vertical_scroll, horizontal_scroll))
                .block(
                    input_block
                        .borders(Borders::ALL)
                        .border_style(self.focus_border_style(FocusPane::Input)),
                ),
            input,
        );
        if !matches!(
            self.overlay,
            Overlay::Command
                | Overlay::ModelPicker(_)
                | Overlay::ThreadPicker(_)
                | Overlay::Processes(_)
        ) && self.view.focus == FocusPane::Input
            && show_cursor
        {
            frame.set_cursor_position((
                input.x + 1 + cursor_column as u16 - horizontal_scroll,
                input.y + 1 + cursor_row as u16 - vertical_scroll,
            ));
        }
    }

    fn render_overlays(&mut self, frame: &mut Frame<'_>, main: Rect) {
        self.layout.command_input_area = Rect::default();
        self.layout.command_input_scroll = 0;
        if matches!(self.overlay, Overlay::Command) {
            self.render_command_input(frame);
        }
        if let Overlay::ModelPicker(picker) = &self.overlay {
            render_model_picker(frame, picker);
        }
        if let Overlay::ThreadPicker(picker) = &self.overlay {
            render_thread_picker(frame, picker, self.controller_subscription.state());
        }
        if let Overlay::Processes(picker) = &self.overlay {
            render_process_picker(
                frame,
                picker,
                self.processes(),
                self.process_subscription
                    .as_ref()
                    .map(|subscription| subscription.state()),
            );
        }
        if let Some(picker) = self.target.checkpoint_picker() {
            let [list, _] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(30), Constraint::Min(20)])
                .areas(main);
            render_checkpoint_picker(
                frame,
                list,
                picker,
                self.checkpoints(),
                self.view.focus == FocusPane::Checkpoints,
            );
        }
        if matches!(self.overlay, Overlay::HistoryConfirmation(_)) {
            render_history_confirmation(frame);
        }
        if matches!(self.overlay, Overlay::LoadingCheckpoints) {
            render_message_overlay(frame, "Checkpoints", "Loading checkpoints…", "Esc close");
        }
        if matches!(self.overlay, Overlay::NoCheckpoints) {
            render_message_overlay(frame, "Checkpoints", "No checkpoints", "Enter/Esc close");
        }
        if let Overlay::Operation(operation) = &self.overlay {
            let message = match operation {
                OperationOverlay::RenamingThread => "Renaming thread…",
                OperationOverlay::ChangingModel => "Changing model…",
                OperationOverlay::CreatingCheckpoint => "Creating checkpoint…",
                OperationOverlay::ForkingThread => "Forking thread…",
                OperationOverlay::RewindingThread => "Rewinding thread…",
                OperationOverlay::RestoringCheckpoint => "Restoring checkpoint…",
            };
            render_message_overlay(frame, "Please wait", message, "");
        }
        if matches!(self.overlay, Overlay::Help) {
            render_command_help(frame);
        }
    }

    fn render_command_input(&mut self, frame: &mut Frame<'_>) {
        let width = frame
            .area()
            .width
            .saturating_sub(8)
            .clamp(3, 72)
            .min(frame.area().width);
        let area = Rect::new(
            frame.area().x + (frame.area().width - width) / 2,
            frame.area().y + frame.area().height.saturating_sub(3) / 2,
            width,
            3,
        );
        let block = Block::default()
            .title("Command")
            .title_bottom(Line::from("Enter run · Esc close").right_aligned())
            .borders(Borders::ALL);
        let inner = block.inner(area);
        let input_area = Rect::new(
            inner.x.saturating_add(1),
            inner.y,
            inner.width.saturating_sub(1),
            inner.height,
        );
        let cursor_column =
            expand_tabs(&self.command_input.value[..self.command_input.cursor]).width();
        let horizontal_scroll =
            cursor_column.saturating_sub(usize::from(input_area.width).saturating_sub(1)) as u16;
        self.layout.command_input_area = input_area;
        self.layout.command_input_scroll = horizontal_scroll;
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new("/"), inner);
        frame.render_widget(
            Paragraph::new(selected_input_text(
                &self.command_input.value,
                self.command_input.selection_range(),
            ))
            .scroll((0, horizontal_scroll)),
            input_area,
        );
        frame.set_cursor_position((
            input_area.x + cursor_column as u16 - horizontal_scroll,
            input_area.y,
        ));
    }

    fn focus_border_style(&self, pane: FocusPane) -> Style {
        if self.overlay.is_none()
            && self.error.is_none()
            && self.pending_approval().is_none()
            && !matches!(self.turn, TurnState::EnteringDenyReason { .. })
            && self.view.focus == pane
        {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        }
    }

    fn status_line(&self) -> Line<'static> {
        let mut spans = self.current_status();
        if !spans.is_empty() {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }
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
        let Some((_, model, effort)) = selected else {
            spans.push(Span::raw("model — · context — · cache —"));
            return Line::from(spans);
        };
        let usage = self.displayed_events().iter().rev().find_map(|event| {
            if let ThreadEventData::ModelRequest(request) = &event.data
                && request.kind == ModelRequestKind::Response
            {
                self.usage_for(event.sequence).map(|usage| (event, usage))
            } else {
                None
            }
        });
        let (context, cache) = usage.map_or_else(
            || ("—".to_owned(), "—".to_owned()),
            |(request, usage)| {
                let input = usage["input_tokens"].as_f64().unwrap_or_default();
                let context = match &request.data {
                    ThreadEventData::ModelRequest(request) => {
                        request.context_window.map(|value| value as f64)
                    }
                    _ => None,
                }
                .filter(|window| *window > 0.0)
                .map_or_else(
                    || "—".to_owned(),
                    |window| format!("{:.0}%", input / window * 100.0),
                );
                let cache = if input > 0.0 {
                    format!(
                        "{:.0}%",
                        usage["cached_input_tokens"].as_f64().unwrap_or_default() / input * 100.0
                    )
                } else {
                    "—".to_owned()
                };
                (context, cache)
            },
        );
        spans.extend([
            Span::styled(
                format!("{model} ({effort})"),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::raw(" · "),
            Span::styled(
                format!("context {context}"),
                Style::default().fg(Color::Blue),
            ),
            Span::raw(" · "),
            Span::styled(
                format!("cache {cache}"),
                Style::default().fg(Color::LightMagenta),
            ),
        ]);
        spans.extend(self.quota_status());
        let running_processes = self
            .processes()
            .iter()
            .filter(|process| matches!(process.status(), ProcessStatus::Running))
            .count();
        if running_processes != 0 {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                format!("processes {running_processes}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        if let Some(checkpoint) = self.checkpoint() {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                format!("checkpoint {} ({})", checkpoint.id, checkpoint.reason),
                Style::default().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    }

    fn thread_breadcrumb(&self) -> Option<Line<'static>> {
        if let Some(current) = self.target.thread_id() {
            let mut path = Vec::new();
            let mut id = Some(current);
            while let Some(current) = id {
                let Some(thread) = self.threads().iter().find(|thread| thread.id == current) else {
                    break;
                };
                path.push(breadcrumb_name(thread.display_name.as_deref()));
                id = thread.parent_thread_id;
            }
            path.reverse();
            if !path.is_empty() {
                return Some(Line::from(vec![
                    Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
                    Span::raw(path.join(" › ")),
                ]));
            }
        }
        None
    }

    fn current_status(&self) -> Vec<Span<'static>> {
        let (message, color) = match &self.turn {
            TurnState::Starting {
                phase: TurnPhase::Compacting,
            } => ("Compacting".to_owned(), Color::Yellow),
            TurnState::Starting { .. } => ("Starting".to_owned(), Color::Yellow),
            TurnState::Cancelling => ("Cancelling".to_owned(), Color::Yellow),
            TurnState::ResolvingApproval { .. } => ("Resuming".to_owned(), Color::Yellow),
            TurnState::AnsweringQuestions(form) => {
                let message = if form.mode == QuestionFormMode::Submitting {
                    "Sending answers"
                } else {
                    "Answer required"
                };
                (message.to_owned(), Color::Yellow)
            }
            TurnState::EnteringDenyReason { .. } => ("Approval required".to_owned(), Color::Yellow),
            TurnState::Idle => {
                let Some(turn) = self.active_turn() else {
                    return Vec::new();
                };
                match turn.phase() {
                    TurnPhase::Retrying => {
                        let message = turn.retry().map_or_else(
                            || "Retrying".to_owned(),
                            |retry| {
                                let summary =
                                    expand_tabs(&sanitize(retry.summary())).replace('\n', " ");
                                format!("{summary}: retrying {}/{}", retry.current(), retry.max())
                            },
                        );
                        (message, Color::Red)
                    }
                    TurnPhase::AwaitingInput => ("Input required".to_owned(), Color::Yellow),
                    TurnPhase::Cancelling => ("Cancelling".to_owned(), Color::Yellow),
                    TurnPhase::Compacting => ("Compacting".to_owned(), Color::Yellow),
                    TurnPhase::Running => ("Working".to_owned(), Color::Yellow),
                }
            }
        };
        let mut spans = vec![Span::styled(message, Style::default().fg(color))];
        if self.turn_is_running()
            && !matches!(
                self.turn,
                TurnState::Cancelling | TurnState::AnsweringQuestions(_)
            )
        {
            spans.push(Span::styled(
                " · esc cancel",
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans
    }

    fn quota_status(&self) -> Vec<Span<'static>> {
        let snapshots = self
            .selected_rate_limits()
            .and_then(serde_json::Value::as_array);
        let Some(snapshot) = snapshots.and_then(|snapshots| {
            snapshots
                .iter()
                .rev()
                .find(|snapshot| snapshot["limit_id"] == "codex")
                .or_else(|| snapshots.last())
        }) else {
            return Vec::new();
        };
        let first_snapshot = self
            .displayed_events()
            .iter()
            .find_map(|event| match &event.data {
                ThreadEventData::RateLimits(event) => event.snapshots.as_array(),
                _ => None,
            })
            .and_then(|snapshots| {
                snapshots
                    .iter()
                    .find(|candidate| candidate["limit_id"] == snapshot["limit_id"])
                    .or_else(|| snapshots.first())
            });
        let windows = ["primary", "secondary"]
            .into_iter()
            .filter_map(|name| {
                let current = snapshot.get(name)?;
                has_quota_window(current).then(|| {
                    let delta = first_snapshot
                        .and_then(|first| first.get(name))
                        .and_then(|first| quota_delta(first, current));
                    (name, format_quota_window(current, delta))
                })
            })
            .collect::<Vec<_>>();
        let credits = snapshot
            .pointer("/credits/balance")
            .and_then(Value::as_str)
            .filter(|balance| balance.parse::<f64>().is_ok_and(|balance| balance > 0.0));
        if windows.is_empty() && credits.is_none() {
            return Vec::new();
        }
        let mut spans = vec![Span::raw(" · ")];
        let mut has_detail = false;
        let mut deltas = Vec::new();
        for (index, (name, (window, delta))) in windows.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" · "));
            }
            let color = if name == "primary" {
                Color::Cyan
            } else {
                Color::Green
            };
            spans.push(Span::styled(window, Style::default().fg(color)));
            deltas.extend(delta);
            has_detail = true;
        }
        if !deltas.is_empty() {
            spans.push(Span::raw(" · "));
            let deltas = deltas
                .into_iter()
                .map(|(label, delta)| format!("{label} +{delta:.1}pt"))
                .collect::<Vec<_>>()
                .join(" ");
            spans.push(Span::styled(
                format!("thread {deltas}"),
                Style::default().fg(Color::Magenta),
            ));
        }
        if let Some(balance) = credits {
            if has_detail {
                spans.push(Span::raw(" · "));
            }
            spans.push(Span::styled(
                format!("credits {balance}"),
                Style::default().fg(Color::Blue),
            ));
        }
        spans
    }

    fn render_coding_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .title("Transcript")
            .borders(Borders::ALL)
            .border_style(self.focus_border_style(FocusPane::Transcript));
        let inner = block.inner(area);
        prepare_transcript(
            &mut self.transcript.entries,
            &self.view.expanded_tools,
            inner.width,
        );
        let (content_length, item_ranges) = transcript_ranges(&self.transcript.entries);
        let max_scroll = content_length.saturating_sub(usize::from(inner.height));
        self.view.transcript_scroll = preserve_transcript_viewport(
            self.view.transcript_scroll,
            self.layout.transcript_max_scroll,
            max_scroll,
        );
        self.layout.transcript_max_scroll = max_scroll;
        let scroll = max_scroll.saturating_sub(self.view.transcript_scroll);
        let lines = transcript_lines(
            &self.transcript.entries,
            self.selection_range(),
            self.view.selected_item,
            inner.width,
            scroll..scroll + usize::from(inner.height),
        );
        self.layout.transcript_item_ranges = item_ranges.clone();
        self.layout.item_areas = item_ranges
            .into_iter()
            .filter_map(|(index, rows)| {
                let start = rows.start.saturating_sub(scroll);
                let end = rows.end.saturating_sub(scroll);
                (start < usize::from(inner.height) && end > 0).then_some((
                    index,
                    Rect::new(
                        inner.x,
                        inner.y + start.min(usize::from(inner.height)) as u16,
                        inner.width,
                        (end.min(usize::from(inner.height)) - start.min(usize::from(inner.height)))
                            .max(1) as u16,
                    ),
                ))
            })
            .collect();
        self.layout.transcript = layout_transcript(&self.transcript.entries, inner, scroll);
        frame.render_widget(Paragraph::new(lines).block(block), area);
        if max_scroll > 0 && inner.height > 2 {
            self.layout.transcript_scrollbar_area =
                Rect::new(area.right() - 1, inner.y, 1, inner.height);
            let scrollbar_position =
                scroll.saturating_mul(content_length.saturating_sub(1)) / max_scroll;
            let track_height = inner.height.saturating_sub(2);
            let denominator = content_length
                .saturating_sub(1)
                .saturating_add(usize::from(inner.height));
            let thumb_len = rounded_divide(
                usize::from(inner.height).saturating_mul(usize::from(track_height)),
                denominator,
            )
            .clamp(1, usize::from(track_height)) as u16;
            let thumb_start = rounded_divide(
                scrollbar_position.saturating_mul(usize::from(track_height)),
                denominator,
            )
            .min(usize::from(track_height.saturating_sub(thumb_len)))
                as u16;
            self.layout.transcript_scrollbar_thumb_start = thumb_start;
            self.layout.transcript_scrollbar_thumb_len = thumb_len;
            let mut scrollbar_state =
                ScrollbarState::new(content_length).position(scrollbar_position);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        } else {
            self.layout.transcript_scrollbar_area = Rect::default();
            self.layout.transcript_scrollbar_drag_offset = None;
        }
    }

    fn usage_for(
        &self,
        request_sequence: atra_protocol::EventSequence,
    ) -> Option<&serde_json::Value> {
        self.displayed_events()
            .iter()
            .find_map(|event| match &event.data {
                ThreadEventData::TokenUsage(event)
                    if event.request_sequence == request_sequence =>
                {
                    Some(&event.usage)
                }
                _ => None,
            })
    }
}

fn breadcrumb_name(name: Option<&str>) -> String {
    name.map(|name| truncate_display_width(&expand_tabs(&sanitize(name)).replace('\n', " "), 24))
        .unwrap_or_else(|| "Untitled".to_owned())
}

fn truncate_display_width(value: &str, max_width: usize) -> String {
    if value.width() <= max_width {
        return value.to_owned();
    }
    let content_width = max_width.saturating_sub(1);
    let mut result = String::new();
    let mut width = 0;
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push('…');
    result
}

fn question_form_height(form: &QuestionForm, available: u16) -> u16 {
    let desired = if matches!(
        form.mode,
        QuestionFormMode::Confirm | QuestionFormMode::Submitting
    ) {
        form.request.questions.len().saturating_mul(4) + 4
    } else {
        form.request.questions[form.current]
            .options
            .len()
            .saturating_add(1)
            .saturating_mul(2)
            + 5
    };
    (desired as u16).min(14).min(available)
}

fn render_question_form(frame: &mut Frame<'_>, area: Rect, form: &QuestionForm) {
    if area.is_empty() {
        return;
    }
    let submitting = form.mode == QuestionFormMode::Submitting;
    let title = if submitting {
        " Sending answers "
    } else if form.mode == QuestionFormMode::Confirm {
        " Confirm answers "
    } else {
        " Questions "
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area).inner(Margin {
        vertical: 0,
        horizontal: 1,
    });
    frame.render_widget(block, area);
    let questions = &form.request.questions;

    if matches!(
        form.mode,
        QuestionFormMode::Confirm | QuestionFormMode::Submitting
    ) {
        let mut lines = Vec::new();
        for (index, question) in questions.iter().enumerate() {
            let option = question.options.get(form.drafts[index].selected);
            lines.push(Line::styled(
                format!("{}. {}", index + 1, sanitize(&question.question)),
                Style::default().fg(Color::Cyan),
            ));
            let label = option.map_or_else(
                || "どれでもない".to_owned(),
                |option| sanitize(&option.label),
            );
            let recommended = if option
                .is_some_and(|option| question.recommended_options.contains(&option.label))
            {
                " ★ recommended"
            } else {
                ""
            };
            lines.push(Line::from(format!("   {label}{recommended}")));
            let description = option.map_or_else(
                || "上記の選択肢を選ばない".to_owned(),
                |option| sanitize(&option.description),
            );
            if !description.is_empty() {
                lines.push(Line::styled(
                    format!("   {description}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if !form.drafts[index].note.value.is_empty() {
                lines.push(Line::from("   Note:"));
                lines.extend(
                    form.drafts[index]
                        .note
                        .value
                        .lines()
                        .map(|line| Line::from(format!("     {line}"))),
                );
            }
            lines.push(Line::default());
        }
        let [content, hint] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .areas(inner);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((form.scroll.min(u16::MAX as usize) as u16, 0)),
            content,
        );
        frame.render_widget(
            Paragraph::new(if submitting {
                "Sending…"
            } else {
                "Enter / →: send · ← / Esc: back · ↑ / ↓: scroll"
            })
            .style(Style::default().fg(Color::DarkGray))
            .right_aligned(),
            hint,
        );
        return;
    }

    let question = &questions[form.current];
    let [heading, options, hint] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
    frame.render_widget(
        Paragraph::new(format!(
            "{}/{}  {}",
            form.current + 1,
            questions.len(),
            sanitize(&question.question)
        ))
        .style(Style::default().fg(Color::Cyan))
        .wrap(Wrap { trim: false }),
        heading,
    );
    let mut items = question
        .options
        .iter()
        .map(|option| {
            let recommended = question.recommended_options.contains(&option.label);
            ListItem::new(vec![
                Line::from(vec![
                    Span::raw(sanitize(&option.label)),
                    Span::styled(
                        if recommended {
                            "  ★ recommended".to_owned()
                        } else {
                            String::new()
                        },
                        Style::default().fg(Color::Green),
                    ),
                ]),
                Line::styled(
                    format!("  {}", sanitize(&option.description)),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect::<Vec<_>>();
    items.push(ListItem::new(vec![
        Line::from("どれでもない"),
        Line::styled(
            "  上記の選択肢を選ばない",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    let mut state = ListState::default().with_selected(Some(form.drafts[form.current].selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(Color::Yellow)),
        options,
        &mut state,
    );
    frame.render_widget(
        Paragraph::new(if form.mode == QuestionFormMode::Note {
            "Editing note below · Tab / Esc: return to options"
        } else {
            "↑ / ↓: select · Enter / →: next · ←: previous · Tab: edit note · Esc: cancel"
        })
        .style(Style::default().fg(Color::DarkGray))
        .right_aligned(),
        hint,
    );
}

fn rounded_divide(numerator: usize, denominator: usize) -> usize {
    (numerator + denominator / 2) / denominator
}

fn render_thread_picker(
    frame: &mut Frame<'_>,
    picker: &ThreadPicker,
    controller: &atra_protocol::ControllerState,
) {
    let threads = controller.threads();
    let width = frame.area().width.saturating_sub(8).min(72);
    let visible = crate::state::visible_threads(threads, &picker.collapsed);
    let height = (visible.len() as u16 + 2)
        .min(frame.area().height.saturating_sub(4))
        .max(3);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + (frame.area().height - height) / 2,
        width,
        height,
    );
    let items = visible
        .iter()
        .map(|(thread, depth)| {
            let display_name = thread
                .display_name
                .as_deref()
                .map(|name| expand_tabs(&sanitize(name)).replace('\n', " "))
                .unwrap_or_else(|| "Untitled thread".to_owned());
            let has_children = threads
                .iter()
                .any(|candidate| candidate.parent_thread_id == Some(thread.id));
            let marker = if !has_children {
                "  "
            } else if picker.collapsed.contains(&thread.id) {
                "▸ "
            } else {
                "▾ "
            };
            let aggregate = if picker.collapsed.contains(&thread.id) {
                let mut pending = vec![thread.id];
                let mut running = 0;
                let mut questions = 0;
                let mut approvals = 0;
                while let Some(parent) = pending.pop() {
                    for child in threads
                        .iter()
                        .filter(|candidate| candidate.parent_thread_id == Some(parent))
                    {
                        pending.push(child.id);
                        match controller.thread_status(child.id) {
                            Some(
                                atra_protocol::AgentStatus::Running
                                | atra_protocol::AgentStatus::Compacting
                                | atra_protocol::AgentStatus::Cancelling,
                            ) => running += 1,
                            Some(atra_protocol::AgentStatus::AwaitingQuestion) => questions += 1,
                            Some(atra_protocol::AgentStatus::AwaitingApproval) => approvals += 1,
                            _ => {}
                        }
                    }
                }
                if running + questions + approvals == 0 {
                    String::new()
                } else {
                    format!("  [▶{running} ?{questions} !{approvals}]")
                }
            } else {
                String::new()
            };
            ListItem::new(format!(
                "{}{}{}{}",
                "  ".repeat(*depth),
                marker,
                display_name,
                aggregate
            ))
        })
        .collect::<Vec<_>>();
    let mut state =
        ListState::default().with_selected((!visible.is_empty()).then_some(picker.selected));
    frame.render_widget(Clear, area);
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("● ").block(
            Block::default()
                .title("Select thread")
                .title_bottom(
                    Line::from("↑/↓ select · Enter switch · x delete · Esc close").right_aligned(),
                )
                .borders(Borders::ALL),
        ),
        area,
        &mut state,
    );

    match picker.state {
        ThreadPickerState::ConfirmingDelete => {
            let count = visible.get(picker.selected).map_or(0, |(selected, _)| {
                let mut descendants = vec![selected.id];
                let mut count = 0;
                while let Some(parent) = descendants.pop() {
                    for child in threads
                        .iter()
                        .filter(|thread| thread.parent_thread_id == Some(parent))
                    {
                        count += 1;
                        descendants.push(child.id);
                    }
                }
                count
            });
            render_delete_confirmation(frame, area, count)
        }
        ThreadPickerState::Deleting => render_delete_progress(frame, area),
        ThreadPickerState::Selecting => {
            render_progress(frame, area, "Please wait", "Loading thread…")
        }
        ThreadPickerState::Browsing => {}
    }
}

fn render_delete_confirmation(frame: &mut Frame<'_>, area: Rect, descendants: usize) {
    let width = area.width.saturating_sub(8).min(54);
    let confirmation = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + area.height.saturating_sub(3) / 2,
        width,
        3,
    );
    frame.render_widget(Clear, confirmation);
    frame.render_widget(
        Paragraph::new(if descendants == 0 {
            "[y] Delete thread  [n] Cancel".to_owned()
        } else {
            format!("[y] Delete thread + {descendants} descendants  [n] Cancel")
        })
        .block(
            Block::default()
                .title("Delete thread?")
                .borders(Borders::ALL),
        ),
        confirmation,
    );
}

fn render_delete_progress(frame: &mut Frame<'_>, area: Rect) {
    render_progress(frame, area, "Please wait", "Deleting thread…");
}

fn render_progress(frame: &mut Frame<'_>, area: Rect, title: &str, message: &str) {
    let width = area.width.saturating_sub(8).min(54);
    let progress = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + area.height.saturating_sub(3) / 2,
        width,
        3,
    );
    frame.render_widget(Clear, progress);
    frame.render_widget(
        Paragraph::new(message.to_owned()).block(
            Block::default()
                .title(title.to_owned())
                .borders(Borders::ALL),
        ),
        progress,
    );
}

fn render_message_overlay(frame: &mut Frame<'_>, title: &str, message: &str, footer: &str) {
    let width = frame.area().width.saturating_sub(8).min(54);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + frame.area().height.saturating_sub(3) / 2,
        width,
        3,
    );
    frame.render_widget(Clear, area);
    let mut block = Block::default()
        .title(title.to_owned())
        .borders(Borders::ALL);
    if !footer.is_empty() {
        block = block.title_bottom(Line::from(footer.to_owned()).right_aligned());
    }
    frame.render_widget(Paragraph::new(message.to_owned()).block(block), area);
}

fn render_error(frame: &mut Frame<'_>, error: &anyhow::Error) {
    let message = expand_tabs(&sanitize(&format!("{error:#}")));
    let width = frame.area().width.saturating_sub(8).min(72);
    let height = (message.lines().count() as u16 + 2)
        .clamp(3, 10)
        .min(frame.area().height);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + (frame.area().height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(message).wrap(Wrap { trim: false }).block(
            Block::default()
                .title("Error")
                .title_bottom(Line::from("Enter/Esc close").right_aligned())
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        ),
        area,
    );
}

fn render_process_picker(
    frame: &mut Frame<'_>,
    picker: &ProcessPicker,
    processes: &[atra_protocol::ProcessSummary],
    detail: Option<&atra_protocol::ProcessState>,
) {
    let width = frame.area().width.saturating_sub(4).min(120);
    let height = frame
        .area()
        .height
        .saturating_sub(4)
        .clamp(8, 32)
        .min(frame.area().height);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + (frame.area().height - height) / 2,
        width,
        height,
    );
    let [list_area, detail_area] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .areas(area);
    let items = process_list_items(processes);
    let mut state =
        ListState::default().with_selected((!processes.is_empty()).then_some(picker.selected));
    frame.render_widget(Clear, area);
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("● ").block(
            Block::default()
                .title(format!("Background processes · {}", processes.len()))
                .title_bottom(
                    Line::from("↑/↓ select · PgUp/PgDn output · x stop · Esc close")
                        .right_aligned(),
                )
                .borders(Borders::ALL),
        ),
        list_area,
        &mut state,
    );
    render_process_detail(frame, picker, processes, detail, detail_area);

    match &picker.state {
        ProcessPickerState::ConfirmingStop { .. } => render_stop_confirmation(frame, area),
        ProcessPickerState::Stopping { process_id } => render_progress(
            frame,
            area,
            "Please wait",
            &format!("Stopping {process_id}…"),
        ),
        ProcessPickerState::Browsing => {}
    }
}

fn process_list_items(processes: &[atra_protocol::ProcessSummary]) -> Vec<ListItem<'static>> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64);
    processes
        .iter()
        .map(|process| {
            let (status, color) = match process.status() {
                ProcessStatus::Running => ("running".to_owned(), Color::Green),
                ProcessStatus::Exited { exit_code } => (
                    format!(
                        "exited {}",
                        exit_code.map_or_else(|| "?".to_owned(), |code| code.to_string())
                    ),
                    Color::DarkGray,
                ),
                ProcessStatus::Unavailable { .. } => ("unavailable".to_owned(), Color::Red),
            };
            let elapsed = format_elapsed_ms(now_ms.saturating_sub(process.started_at_ms()));
            let command = expand_tabs(&sanitize(process.command())).replace('\n', " ");
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(status, Style::default().fg(color)),
                    Span::raw(format!(" · {elapsed} · {}", process.locator().process_id())),
                ]),
                Line::styled(
                    format!("  {} · {command}", process.locator().runner()),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect()
}

fn render_process_detail(
    frame: &mut Frame<'_>,
    picker: &ProcessPicker,
    processes: &[atra_protocol::ProcessSummary],
    detail: Option<&atra_protocol::ProcessState>,
    area: Rect,
) {
    let [command_area, output_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(3)])
        .areas(area);
    let selected = processes.get(picker.selected);
    let command = selected
        .map(|process| expand_tabs(&sanitize(process.command())))
        .unwrap_or_else(|| "No background processes".to_owned());
    frame.render_widget(
        Paragraph::new(command).block(Block::default().title("Command").borders(Borders::ALL)),
        command_area,
    );

    let detail = detail.filter(|detail| {
        selected.is_some_and(|selected| detail.process().locator() == selected.locator())
    });
    let output = detail
        .map(|detail| expand_tabs(&sanitize(detail.output_tail())))
        .unwrap_or_default();
    let mut lines = output
        .lines()
        .map(|line| Line::from(line.to_owned()))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        let message = match selected.map(|process| process.status()) {
            Some(ProcessStatus::Unavailable { message }) => expand_tabs(&sanitize(message)),
            Some(_) => "(no output)".to_owned(),
            None => String::new(),
        };
        lines.push(Line::from(message));
    }
    let visible_height = usize::from(output_area.height.saturating_sub(2));
    let bottom = lines.len().saturating_sub(visible_height);
    let start = bottom.saturating_sub(picker.output_scroll);
    let end = (start + visible_height).min(lines.len());
    let omitted_bytes = detail.map_or(0, atra_protocol::ProcessState::omitted_bytes);
    frame.render_widget(
        Paragraph::new(lines[start..end].to_vec()).block(
            Block::default()
                .title(if omitted_bytes == 0 {
                    "Output tail".to_owned()
                } else {
                    format!("Output tail · {omitted_bytes} earlier bytes omitted")
                })
                .borders(Borders::ALL),
        ),
        output_area,
    );
}

fn render_stop_confirmation(frame: &mut Frame<'_>, area: Rect) {
    let width = area.width.saturating_sub(8).min(54);
    let confirmation = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + area.height.saturating_sub(3) / 2,
        width,
        3,
    );
    frame.render_widget(Clear, confirmation);
    frame.render_widget(
        Paragraph::new("[y] Stop process  [n] Cancel").block(
            Block::default()
                .title("Stop background process?")
                .borders(Borders::ALL),
        ),
        confirmation,
    );
}

fn format_elapsed_ms(milliseconds: i64) -> String {
    let seconds = milliseconds.max(0) / 1000;
    if seconds >= 60 * 60 {
        format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn render_checkpoint_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &CheckpointPicker,
    checkpoints: &[atra_protocol::ThreadCheckpoint],
    focused: bool,
) {
    let items = checkpoints
        .iter()
        .map(|checkpoint| {
            ListItem::new(format!(
                "#{} · {} · {}",
                checkpoint.id, checkpoint.reason, checkpoint.created_at_ms
            ))
        })
        .collect::<Vec<_>>();
    let selected = checkpoints
        .iter()
        .position(|checkpoint| checkpoint.id == picker.selected);
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("● ").block(
            Block::default()
                .title(if picker.loading {
                    "Checkpoints · Loading…"
                } else {
                    "Checkpoints"
                })
                .title_bottom(Line::from("Tab transcript · Esc return").right_aligned())
                .borders(Borders::ALL)
                .border_style(if focused {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                }),
        ),
        area,
        &mut state,
    );
}

fn render_history_confirmation(frame: &mut Frame<'_>) {
    let width = frame.area().width.saturating_sub(8).min(52);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + frame.area().height.saturating_sub(3) / 2,
        width,
        3,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new("[y] Confirm  [n] Cancel").block(
            Block::default()
                .title("Change thread history")
                .borders(Borders::ALL),
        ),
        area,
    );
}

pub(super) fn render_model_picker(frame: &mut Frame<'_>, picker: &ModelPicker) {
    let width = frame.area().width.saturating_sub(8).min(72);
    let height = frame.area().height.saturating_sub(4).min(18);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + (frame.area().height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    match picker.stage {
        ModelPickerStage::Provider => {
            let providers = picker.providers();
            let items = providers
                .iter()
                .map(|provider| {
                    let count = picker
                        .models
                        .iter()
                        .filter(|model| model.provider == **provider)
                        .count();
                    ListItem::new(format!("{} ({count})", sanitize(provider)))
                })
                .collect::<Vec<_>>();
            let mut state = ListState::default()
                .with_selected((!providers.is_empty()).then_some(picker.provider_index));
            frame.render_stateful_widget(
                List::new(items).highlight_symbol("● ").block(
                    Block::default()
                        .title("Select provider")
                        .title_bottom(
                            Line::from("↑/↓ select · Enter choose · Esc close").right_aligned(),
                        )
                        .borders(Borders::ALL),
                ),
                area,
                &mut state,
            );
        }
        ModelPickerStage::Model => {
            let visible = picker.visible_model_indices();
            let selected = visible
                .iter()
                .position(|index| *index == picker.model_index);
            let mut items = visible
                .iter()
                .map(|index| {
                    let model = &picker.models[*index];
                    let description = model.description.as_deref().unwrap_or_default();
                    ListItem::new(vec![
                        Line::from(sanitize(&format!("{} · {}", model.display_name, model.id))),
                        Line::from(Span::styled(
                            format!("  {}", sanitize(description)),
                            Style::default().fg(Color::DarkGray),
                        )),
                    ])
                })
                .collect::<Vec<_>>();
            if items.is_empty() {
                items.push(ListItem::new("No matching models"));
            }
            let provider = picker
                .providers()
                .get(picker.provider_index)
                .map_or_else(String::new, |provider| sanitize(provider));
            let title = if picker.query.is_empty() {
                format!("Select model · {provider}")
            } else {
                format!(
                    "Select model · {provider} · Search: {}",
                    sanitize(&picker.query)
                )
            };
            let mut state = ListState::default().with_selected(selected);
            frame.render_stateful_widget(
                List::new(items).highlight_symbol("● ").block(
                    Block::default()
                        .title(title)
                        .title_bottom(
                            Line::from("Type search · ↑/↓ select · Enter continue · Esc back")
                                .right_aligned(),
                        )
                        .borders(Borders::ALL),
                ),
                area,
                &mut state,
            );
        }
        ModelPickerStage::Effort => {
            let Some(selected_model) = picker.selected_model() else {
                return;
            };
            let items = selected_model
                .supported_reasoning_efforts
                .iter()
                .map(|effort| ListItem::new(effort.as_str()))
                .collect::<Vec<_>>();
            let mut state = ListState::default().with_selected(Some(picker.effort_index));
            frame.render_stateful_widget(
                List::new(items).highlight_symbol("● ").block(
                    Block::default()
                        .title(format!(
                            "Reasoning effort · {}",
                            sanitize(&selected_model.display_name)
                        ))
                        .title_bottom(
                            Line::from("↑/↓ select · Enter apply · Esc back").right_aligned(),
                        )
                        .borders(Borders::ALL),
                ),
                area,
                &mut state,
            );
        }
    }
}

fn render_command_help(frame: &mut Frame<'_>) {
    let width = frame.area().width.saturating_sub(4).min(58);
    let height = (COMMAND_HELP.len() as u16 + 4).min(frame.area().height);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + (frame.area().height - height) / 2,
        width,
        height,
    );
    let lines = COMMAND_HELP
        .iter()
        .map(|(command, description)| {
            Line::from(vec![
                Span::styled(
                    format!("{command:<9}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
                Span::raw(*description),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title("Commands")
                .title_bottom(Line::from("Enter/Esc close").right_aligned())
                .borders(Borders::ALL),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quota_window_accepts_float_encoded_timing_values() {
        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as f64
            + 3600.0;
        let window = json!({
            "used_percent": 25.0,
            "window_minutes": 10080.0,
            "resets_at": reset
        });

        let (content, _) = format_quota_window(&window, None);

        assert!(content.starts_with("weekly 75.000%/"));
        assert!(!content.ends_with("/?"));
    }

    #[test]
    fn quota_window_requires_usage_data() {
        assert!(!has_quota_window(&json!(null)));
        assert!(!has_quota_window(&json!({
            "used_percent": null,
            "window_minutes": null,
            "resets_at": null
        })));
        assert!(has_quota_window(&json!({"used_percent": 0.0})));
    }

    #[test]
    fn breadcrumb_name_is_single_line_and_removes_terminal_sequences() {
        assert_eq!(
            breadcrumb_name(Some("root\x1b[31m\nchild\tname")),
            "root child   name"
        );
    }

    #[test]
    fn breadcrumb_name_is_truncated_to_24_display_columns() {
        assert_eq!(
            breadcrumb_name(Some("1234567890123456789012345")),
            "12345678901234567890123…"
        );
        assert_eq!(
            breadcrumb_name(Some("日本語日本語日本語日本語日本語")),
            "日本語日本語日本語日本…"
        );
    }
}
