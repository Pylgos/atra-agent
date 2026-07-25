use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
};
use unicode_width::UnicodeWidthStr;

use crate::app::{
    App, FocusPane, ModelPicker, TranscriptMode, layout_transcript, prepare_transcript, sanitize,
    transcript_lines, transcript_ranges,
};

impl App {
    pub(super) fn render(&mut self, frame: &mut Frame<'_>) {
        let input_height = (self.input.bytes().filter(|byte| *byte == b'\n').count() as u16 + 3)
            .min(frame.area().height.saturating_sub(5).max(3));
        let [main, input, status] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(4),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .areas(frame.area());
        let [sidebar, transcript] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(18), Constraint::Min(20)])
            .areas(main);
        self.sidebar = sidebar;
        self.input_area = input;

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

        self.transcript_area = transcript;
        match self.transcript_mode {
            TranscriptMode::Coding => self.render_coding_transcript(frame, transcript),
            TranscriptMode::Debug => self.render_debug_transcript(frame, transcript),
        }

        let input_title = if self.renaming {
            "Thread name".to_owned()
        } else {
            match &self.approval {
                Some(approval) => format!("Approval: {}", approval.description),
                None => "Message".to_owned(),
            }
        };
        let input_before_cursor = &self.input[..self.input_cursor];
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
            Paragraph::new(self.input.as_str())
                .scroll((vertical_scroll, horizontal_scroll))
                .block(
                    Block::default()
                        .title(input_title)
                        .borders(Borders::ALL)
                        .border_style(self.focus_border_style(FocusPane::Input)),
                ),
            input,
        );
        frame.render_widget(Paragraph::new(self.status.as_str()), status);
        if self.approval.is_none() && self.model_picker.is_none() && self.focus == FocusPane::Input
        {
            frame.set_cursor_position((
                input.x + 1 + cursor_column as u16 - horizontal_scroll,
                input.y + 1 + cursor_row as u16 - vertical_scroll,
            ));
        }
        if let Some(picker) = &self.model_picker {
            render_model_picker(frame, picker);
        }
    }

    fn focus_border_style(&self, pane: FocusPane) -> Style {
        if self.approval.is_none() && self.model_picker.is_none() && self.focus == pane {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        }
    }

    fn render_coding_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.request_list_area = Rect::default();
        self.detail_area = Rect::default();
        let block = Block::default()
            .title("Transcript")
            .borders(Borders::ALL)
            .border_style(self.focus_border_style(FocusPane::Transcript));
        let inner = block.inner(area);
        prepare_transcript(&mut self.transcript, &self.expanded_tools, inner.width);
        let (content_length, item_ranges) = transcript_ranges(&self.transcript);
        let max_scroll = content_length.saturating_sub(usize::from(inner.height));
        self.transcript_max_scroll = max_scroll;
        self.transcript_scroll = self.transcript_scroll.min(max_scroll);
        let scroll = max_scroll.saturating_sub(self.transcript_scroll);
        let lines = transcript_lines(
            &self.transcript,
            self.selection_range(),
            self.selected_item,
            inner.width,
            scroll..scroll + usize::from(inner.height),
        );
        self.transcript_item_ranges = item_ranges.clone();
        self.item_areas = item_ranges
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
        self.transcript_layout = layout_transcript(&self.transcript, inner, scroll);
        frame.render_widget(Paragraph::new(lines).block(block), area);
        if max_scroll > 0 && inner.height > 2 {
            self.transcript_scrollbar_area = Rect::new(area.right() - 1, inner.y, 1, inner.height);
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
            self.transcript_scrollbar_thumb_start = thumb_start;
            self.transcript_scrollbar_thumb_len = thumb_len;
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
            self.transcript_scrollbar_area = Rect::default();
            self.transcript_scrollbar_drag_offset = None;
        }
    }

    fn render_debug_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.item_areas.clear();
        self.transcript_scrollbar_area = Rect::default();
        self.transcript_max_scroll = 0;
        self.transcript_scrollbar_drag_offset = None;
        let [requests, detail] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(27), Constraint::Min(20)])
            .areas(area);
        self.request_list_area = requests;
        self.detail_area = detail;
        let request_events = self
            .events
            .iter()
            .filter(|event| event.kind == "model_request")
            .collect::<Vec<_>>();
        if !request_events.is_empty() && self.selected_request.is_none() {
            self.selected_request = Some(request_events.len() - 1);
        }
        let selected = self
            .selected_request
            .unwrap_or(0)
            .min(request_events.len().saturating_sub(1));
        self.selected_request = (!request_events.is_empty()).then_some(selected);
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
        self.detail_scroll = self.detail_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((self.detail_scroll as u16, 0))
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
        if self.raw_request {
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
