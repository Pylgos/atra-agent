use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, ModelPicker, layout_transcript, sanitize, transcript_lines};

impl App {
    pub(super) fn render(&mut self, frame: &mut Frame<'_>) {
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
