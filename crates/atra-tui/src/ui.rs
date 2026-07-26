use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState,
    },
};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{Activity, App, COMMAND_HELP},
    state::{CheckpointPicker, FocusPane, ModelPicker, Overlay, ThreadPicker, TranscriptMode},
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
    let label = window["window_minutes"].as_i64().map_or_else(
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
    let reset = window["resets_at"]
        .as_i64()
        .and_then(|reset| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
            Some(format_window_duration((reset - now).max(0) / 60))
        })
        .unwrap_or_else(|| "?".to_owned());
    let content = format!("{label} {remaining:.0}%/{reset}");
    let delta = delta
        .filter(|delta| *delta > 0.0)
        .map(|delta| (label, delta));
    (content, delta)
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

impl App {
    pub(super) fn render(&mut self, frame: &mut Frame<'_>) {
        let input_height = if matches!(self.overlay, Overlay::Approval(_)) {
            3
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
        let [main, input, activity_area, status] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(4),
                Constraint::Length(input_height),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(frame.area());
        self.layout.input_area = input;
        let transcript_area = if self.checkpoint_picker.is_some() {
            let [_, transcript] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(30), Constraint::Min(20)])
                .areas(main);
            transcript
        } else {
            main
        };
        self.layout.transcript_area = transcript_area;
        match self.view.transcript_mode {
            TranscriptMode::Coding => self.render_coding_transcript(frame, transcript_area),
            TranscriptMode::Debug => self.render_debug_transcript(frame, transcript_area),
        }

        let (input_title, input_hint, input_value, input_cursor, show_cursor) = match &self.overlay
        {
            Overlay::Approval(approval) => match &approval.deny_reason {
                Some(reason) => (
                    "Deny reason (optional)".to_owned(),
                    Some(Line::from("Enter: deny · Esc: back").right_aligned()),
                    reason.value.as_str(),
                    reason.cursor,
                    true,
                ),
                None => {
                    let operation = approval
                        .operation_index
                        .map(|index| format!("Operation {index} · "))
                        .unwrap_or_default();
                    let runner = (!approval.runner.is_empty())
                        .then(|| format!("{} · ", approval.runner))
                        .unwrap_or_default();
                    (
                        format!("Approval required · {operation}{runner}{}", approval.label),
                        None,
                        "[y] Allow  [n] Deny",
                        0,
                        false,
                    )
                }
            },
            Overlay::Rename => (
                "Thread name".to_owned(),
                None,
                self.message_input.value.as_str(),
                self.message_input.cursor,
                true,
            ),
            _ => (
                "Message".to_owned(),
                Some(Line::from("Enter: newline · Ctrl-G: send").right_aligned()),
                self.message_input.value.as_str(),
                self.message_input.cursor,
                true,
            ),
        };
        let mut input_block = Block::default().title(input_title);
        if let Some(input_hint) = input_hint {
            input_block = input_block.title_bottom(input_hint);
        }
        let input_before_cursor = &input_value[..input_cursor];
        let cursor_row = input_before_cursor
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let cursor_column = input_before_cursor
            .rsplit_once('\n')
            .map_or(input_before_cursor, |(_, line)| line)
            .width();
        let visible_input_width = usize::from(input.width.saturating_sub(2));
        let visible_input_height = usize::from(input.height.saturating_sub(2));
        let horizontal_scroll =
            cursor_column.saturating_sub(visible_input_width.saturating_sub(1)) as u16;
        let vertical_scroll =
            cursor_row.saturating_sub(visible_input_height.saturating_sub(1)) as u16;
        frame.render_widget(
            Paragraph::new(input_value)
                .scroll((vertical_scroll, horizontal_scroll))
                .block(
                    input_block
                        .borders(Borders::ALL)
                        .border_style(self.focus_border_style(FocusPane::Input)),
                ),
            input,
        );
        if matches!(self.overlay, Overlay::Command) {
            frame.render_widget(
                Paragraph::new(format!("/{}", self.command_input.value)),
                activity_area,
            );
            frame.set_cursor_position((
                activity_area.x
                    + self.command_input.value[..self.command_input.cursor].width() as u16
                    + 1,
                activity_area.y,
            ));
        } else if let Some(activity) = &self.activity {
            let (message, style) = match activity {
                Activity::Info(message) => (message, Style::default().fg(Color::Yellow)),
                Activity::Error(message) => (message, Style::default().fg(Color::Red)),
            };
            frame.render_widget(Paragraph::new(message.as_str()).style(style), activity_area);
        }
        frame.render_widget(Paragraph::new(self.status_line()), status);
        if !matches!(
            self.overlay,
            Overlay::Command | Overlay::ModelPicker(_) | Overlay::ThreadPicker(_)
        ) && self.view.focus == FocusPane::Input
            && show_cursor
        {
            frame.set_cursor_position((
                input.x + 1 + cursor_column as u16 - horizontal_scroll,
                input.y + 1 + cursor_row as u16 - vertical_scroll,
            ));
        }
        if let Overlay::ModelPicker(picker) = &self.overlay {
            render_model_picker(frame, picker);
        }
        if let Overlay::ThreadPicker(picker) = &self.overlay {
            render_thread_picker(frame, picker, &self.threads);
        }
        if let Some(picker) = &self.checkpoint_picker {
            let [list, _] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(30), Constraint::Min(20)])
                .areas(main);
            render_checkpoint_picker(
                frame,
                list,
                picker,
                self.view.focus == FocusPane::Checkpoints,
            );
        }
        if matches!(self.overlay, Overlay::HistoryConfirmation(_)) {
            render_history_confirmation(frame);
        }
        if matches!(self.overlay, Overlay::Help) {
            render_command_help(frame);
        }
    }

    fn focus_border_style(&self, pane: FocusPane) -> Style {
        if !matches!(
            self.overlay,
            Overlay::Approval(_)
                | Overlay::ModelPicker(_)
                | Overlay::ThreadPicker(_)
                | Overlay::HistoryConfirmation(_)
        ) && self.view.focus == pane
        {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        }
    }

    fn status_line(&self) -> Line<'static> {
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
        let Some((model, effort)) = selected else {
            return Line::from("model — · context — · cache —");
        };
        let usage = (!self.metrics_stale)
            .then(|| {
                self.events.iter().rev().find_map(|event| {
                    if event.kind == "model_request"
                        && event.payload["kind"] == "response"
                        && event
                            .payload
                            .pointer("/request/model")
                            .and_then(|value| value.as_str())
                            == Some(model)
                    {
                        self.usage_for(event.sequence).map(|usage| (event, usage))
                    } else {
                        None
                    }
                })
            })
            .flatten();
        let (context, cache) = usage.map_or_else(
            || ("—".to_owned(), "—".to_owned()),
            |(request, usage)| {
                let input = usage["input_tokens"].as_f64().unwrap_or_default();
                let context = request.payload["context_window"]
                    .as_f64()
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
        let mut spans = vec![
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
        ];
        spans.extend(self.quota_status());
        if let Some(checkpoint) = &self.checkpoint {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                format!("checkpoint {} ({})", checkpoint.id, checkpoint.reason),
                Style::default().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    }

    fn quota_status(&self) -> Vec<Span<'static>> {
        let snapshots = self
            .events
            .iter()
            .rev()
            .find(|event| event.kind == "rate_limits")
            .and_then(|event| event.payload["snapshots"].as_array());
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
            .events
            .iter()
            .find(|event| event.kind == "rate_limits")
            .and_then(|event| event.payload["snapshots"].as_array())
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
                (!current.is_null()).then(|| {
                    let delta = first_snapshot
                        .and_then(|first| first.get(name))
                        .and_then(|first| quota_delta(first, current));
                    format_quota_window(current, delta)
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
        for (index, (window, delta)) in windows.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" · "));
            }
            let color = if index == 0 {
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
        self.layout.request_list_area = Rect::default();
        self.layout.detail_area = Rect::default();
        let block = Block::default()
            .title("Transcript")
            .borders(Borders::ALL)
            .border_style(self.focus_border_style(FocusPane::Transcript));
        let inner = block.inner(area);
        prepare_transcript(&mut self.transcript, &self.view.expanded_tools, inner.width);
        let (content_length, item_ranges) = transcript_ranges(&self.transcript);
        let max_scroll = content_length.saturating_sub(usize::from(inner.height));
        self.layout.transcript_max_scroll = max_scroll;
        self.view.transcript_scroll = self.view.transcript_scroll.min(max_scroll);
        let scroll = max_scroll.saturating_sub(self.view.transcript_scroll);
        let lines = transcript_lines(
            &self.transcript,
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
        self.layout.transcript = layout_transcript(&self.transcript, inner, scroll);
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

    fn render_debug_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.layout.item_areas.clear();
        self.layout.transcript_scrollbar_area = Rect::default();
        self.layout.transcript_max_scroll = 0;
        self.layout.transcript_scrollbar_drag_offset = None;
        let [requests, detail] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(27), Constraint::Min(20)])
            .areas(area);
        self.layout.request_list_area = requests;
        self.layout.detail_area = detail;
        let request_events = self
            .events
            .iter()
            .filter(|event| event.kind == "model_request")
            .collect::<Vec<_>>();
        if !request_events.is_empty() && self.view.selected_request.is_none() {
            self.view.selected_request = Some(request_events.len() - 1);
        }
        let selected = self
            .view
            .selected_request
            .unwrap_or(0)
            .min(request_events.len().saturating_sub(1));
        self.view.selected_request = (!request_events.is_empty()).then_some(selected);
        let request_lines = request_events
            .iter()
            .enumerate()
            .flat_map(|(index, event)| {
                let marker = if index == selected { "●" } else { " " };
                let kind = event.payload["kind"].as_str().unwrap_or("request");
                let model = event
                    .payload
                    .pointer("/request/model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let usage = self.usage_for(event.sequence);
                let tokens = usage.map_or_else(
                    || "pending".to_owned(),
                    |usage| {
                        let input = usage["input_tokens"].as_f64().unwrap_or_default();
                        let cached = usage["cached_input_tokens"].as_f64().unwrap_or_default();
                        let hit_rate = if input > 0.0 {
                            cached / input * 100.0
                        } else {
                            0.0
                        };
                        format!(
                            "{} tok · {hit_rate:.0}% cache",
                            usage["total_tokens"].as_i64().unwrap_or_default()
                        )
                    },
                );
                let started = event.payload["started_at_ms"].as_u64().unwrap_or_default();
                let seconds = started / 1_000;
                let millis = started % 1_000;
                [
                    Line::from(Span::styled(
                        format!("{marker} {kind}"),
                        if index == selected {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(ratatui::style::Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    )),
                    Line::from(format!("  {model} · {seconds}.{millis:03}")),
                    Line::from(format!("  {tokens}")),
                ]
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(request_lines).block(
                Block::default()
                    .title("LLM requests")
                    .borders(Borders::ALL)
                    .border_style(self.focus_border_style(FocusPane::Requests)),
            ),
            requests,
        );

        let lines = request_events
            .get(selected)
            .map(|event| self.request_detail_lines(event))
            .unwrap_or_else(|| vec![Line::from("No LLM requests recorded")]);
        let max_scroll = lines
            .len()
            .saturating_sub(usize::from(detail.height.saturating_sub(2)));
        self.view.detail_scroll = self.view.detail_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((self.view.detail_scroll as u16, 0))
                .block(
                    Block::default()
                        .title("Context · r raw/semantic")
                        .borders(Borders::ALL)
                        .border_style(self.focus_border_style(FocusPane::Detail)),
                ),
            detail,
        );
    }

    fn usage_for(&self, request_sequence: i64) -> Option<&serde_json::Value> {
        self.events
            .iter()
            .find(|event| {
                event.kind == "token_usage"
                    && event.payload["request_sequence"].as_i64() == Some(request_sequence)
            })
            .map(|event| &event.payload["usage"])
    }

    fn request_detail_lines(&self, event: &atra_protocol::ThreadEvent) -> Vec<Line<'static>> {
        let request = &event.payload["request"];
        if self.view.raw_request {
            return serde_json::to_string_pretty(request)
                .unwrap_or_else(|_| request.to_string())
                .lines()
                .map(|line| Line::from(line.to_owned()))
                .collect();
        }
        let mut lines = Vec::new();
        let kind = event.payload["kind"].as_str().unwrap_or("request");
        let started = event.payload["started_at_ms"].as_u64().unwrap_or_default();
        let model = request["model"].as_str().unwrap_or("?");
        lines.push(Line::from(Span::styled(
            format!("{kind} · {model}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )));
        lines.push(Line::from(format!("started_at_ms: {started}")));
        lines.push(Line::from(format!(
            "context window: {} · auto compact: {} · compacted: {}",
            value_or_dash(&event.payload["context_window"]),
            value_or_dash(&event.payload["auto_compact_token_limit"]),
            event.payload["compacted"].as_bool().unwrap_or(false),
        )));
        if let Some(usage) = self.usage_for(event.sequence) {
            let input = usage["input_tokens"].as_f64().unwrap_or_default();
            let cached = usage["cached_input_tokens"].as_f64().unwrap_or_default();
            let hit_rate = if input > 0.0 {
                cached / input * 100.0
            } else {
                0.0
            };
            let utilization = event.payload["context_window"]
                .as_f64()
                .filter(|window| *window > 0.0)
                .map(|window| input / window * 100.0);
            lines.push(Line::from(format!(
                "tokens: input {} · cached {} ({hit_rate:.1}%) · cache-write {} · output {} · reasoning {} · total {} · context {}",
                value_or_dash(&usage["input_tokens"]),
                value_or_dash(&usage["cached_input_tokens"]),
                value_or_dash(&usage["cache_write_input_tokens"]),
                value_or_dash(&usage["output_tokens"]),
                value_or_dash(&usage["reasoning_output_tokens"]),
                value_or_dash(&usage["total_tokens"]),
                utilization.map_or_else(|| "—".to_owned(), |value| format!("{value:.1}%")),
            )));
        }
        let input = request["input"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let message_count = input
            .iter()
            .filter(|item| item["type"] == "message")
            .count();
        let tool_count = input
            .iter()
            .filter(|item| {
                matches!(
                    item["type"].as_str(),
                    Some(
                        "function_call"
                            | "function_call_output"
                            | "custom_tool_call"
                            | "custom_tool_call_output"
                    )
                )
            })
            .count();
        let reasoning_count = input
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .count();
        lines.push(Line::from(format!(
            "items: {} · messages {message_count} · tools {tool_count} · reasoning {reasoning_count}",
            input.len()
        )));
        lines.push(Line::from(format!(
            "serialized: request {} B · instructions {} chars/{} B · tools {} B",
            serde_json::to_vec(request).map_or(0, |value| value.len()),
            request["instructions"]
                .as_str()
                .map_or(0, |text| text.chars().count()),
            request["instructions"].as_str().map_or(0, str::len),
            serde_json::to_vec(&request["tools"]).map_or(0, |value| value.len()),
        )));
        lines.push(Line::default());
        lines.push(section("Instructions"));
        lines.extend(
            request["instructions"]
                .as_str()
                .unwrap_or("")
                .lines()
                .map(|line| Line::from(line.to_owned())),
        );
        lines.push(Line::default());
        lines.push(section("Input"));
        for (index, item) in input.iter().enumerate() {
            let encoded = serde_json::to_string_pretty(item).unwrap_or_else(|_| item.to_string());
            lines.push(Line::from(Span::styled(
                format!(
                    "[{index}] {} · {} chars/{} B",
                    item["type"].as_str().unwrap_or("item"),
                    encoded.chars().count(),
                    encoded.len()
                ),
                Style::default().fg(Color::Yellow),
            )));
            lines.extend(encoded.lines().map(|line| Line::from(line.to_owned())));
        }
        lines.push(Line::default());
        lines.push(section("Tools"));
        lines.extend(
            serde_json::to_string_pretty(&request["tools"])
                .unwrap_or_else(|_| request["tools"].to_string())
                .lines()
                .map(|line| Line::from(line.to_owned())),
        );
        lines
    }
}

fn rounded_divide(numerator: usize, denominator: usize) -> usize {
    (numerator + denominator / 2) / denominator
}

fn section(name: &str) -> Line<'static> {
    Line::from(Span::styled(
        name.to_owned(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(ratatui::style::Modifier::BOLD),
    ))
}

fn value_or_dash(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "—".to_owned(),
        value => value.to_string(),
    }
}

fn render_thread_picker(
    frame: &mut Frame<'_>,
    picker: &ThreadPicker,
    threads: &[atra_protocol::Thread],
) {
    let width = frame.area().width.saturating_sub(8).min(72);
    let height = (threads.len() as u16 + 2)
        .min(frame.area().height.saturating_sub(4))
        .max(3);
    let area = Rect::new(
        frame.area().x + (frame.area().width - width) / 2,
        frame.area().y + (frame.area().height - height) / 2,
        width,
        height,
    );
    let items = threads
        .iter()
        .map(|thread| {
            let display_name = thread
                .display_name
                .as_deref()
                .map(|name| sanitize(name).replace(['\n', '\t'], " "))
                .unwrap_or_else(|| "Untitled thread".to_owned());
            ListItem::new(display_name)
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(picker.selected));
    frame.render_widget(Clear, area);
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("● ").block(
            Block::default()
                .title("Select thread")
                .title_bottom(Line::from("Enter switches · Esc cancels").right_aligned())
                .borders(Borders::ALL),
        ),
        area,
        &mut state,
    );
}

fn render_checkpoint_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &CheckpointPicker,
    focused: bool,
) {
    let items = picker
        .checkpoints
        .iter()
        .map(|checkpoint| {
            ListItem::new(format!(
                "#{} · {} · {}",
                checkpoint.id, checkpoint.reason, checkpoint.created_at_ms
            ))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(picker.selected));
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("● ").block(
            Block::default()
                .title("Checkpoints")
                .title_bottom(Line::from("Tab: transcript · Esc: return").right_aligned())
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
                .title_bottom(Line::from("Esc/Enter closes").right_aligned())
                .borders(Borders::ALL),
        ),
        area,
    );
}
