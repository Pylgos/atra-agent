use std::{collections::HashSet, sync::LazyLock};

use atra_patch::{
    ApplyPatchResult, DiffLineKind, FileDiff, PatchOperationOutcome, PatchOperationResult,
};
use atra_protocol::{CommandExecutionArtifact, RunnerOperationArtifact, ToolArtifact};
use ratatui::{
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
use tui_markdown::{Options as MarkdownOptions, StyleSheet, from_str_with_options};
use unicode_width::UnicodeWidthChar;

use crate::{
    layout::{MappedRow, TranscriptLayout},
    transcript::{
        Author, DisplayedLine, RenderedItem, RunnerResult, TranscriptEntry, TranscriptItem,
    },
};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

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

pub(crate) fn transcript_text(entries: &[TranscriptEntry]) -> String {
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
    if let TranscriptItem::ReasoningSummary { text } = item {
        let mut first_event_line = true;
        return render_markdown(text)
            .into_iter()
            .flat_map(|mut line| {
                line.style = line.style.fg(Color::DarkGray);
                for span in &mut line.spans {
                    span.style = span.style.fg(Color::DarkGray);
                }
                wrap_line(line, content_width)
                    .into_iter()
                    .enumerate()
                    .collect::<Vec<_>>()
            })
            .map(|(wrap_index, line)| {
                let displayed = DisplayedLine {
                    marker: first_event_line.then_some('·'),
                    line,
                    continuation: wrap_index != 0,
                };
                first_event_line = false;
                displayed
            })
            .collect();
    }
    let mut logical_lines = match item {
        TranscriptItem::WebSearch { action } => web_search_lines(action),
        TranscriptItem::ToolCall { name, arguments } => tool_call_lines(name, arguments.as_ref()),
        TranscriptItem::RunnerTool {
            input,
            results,
            pending_approval,
            masked,
            ..
        } => runner_tool_lines(input, results, *pending_approval, *masked, expanded),
        TranscriptItem::ToolResult { artifacts, masked } => {
            let mut lines = if *masked {
                vec![(
                    Some('◇'),
                    Line::from(Span::styled(
                        "model context uses masked output",
                        Style::default().fg(Color::DarkGray),
                    )),
                )]
            } else {
                Vec::new()
            };
            lines.extend(fold_result_lines(
                artifacts
                    .iter()
                    .flat_map(|artifact| match artifact {
                        ToolArtifact::RunnerOperation(operation) => {
                            runner_operation_lines(operation)
                        }
                        ToolArtifact::CommandExecution(command) => {
                            command_execution_lines(command, false)
                        }
                        ToolArtifact::PatchOperations(result) => patch_operation_lines(result),
                    })
                    .collect(),
                expanded,
            ));
            lines
        }
        TranscriptItem::Compaction => {
            vec![(Some('·'), Line::from("Earlier context compacted"))]
        }
        TranscriptItem::Message { .. } | TranscriptItem::ReasoningSummary { .. } => unreachable!(),
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
        TranscriptItem::ReasoningSummary { .. } => Style::default().fg(Color::DarkGray),
        TranscriptItem::WebSearch { .. } => Style::default().fg(Color::Blue),
        TranscriptItem::ToolCall { .. } | TranscriptItem::RunnerTool { .. } => {
            Style::default().fg(Color::Yellow)
        }
        TranscriptItem::ToolResult { .. } => Style::default().fg(Color::DarkGray),
        TranscriptItem::Compaction => Style::default().fg(Color::DarkGray),
    };
    if selected {
        style.add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        style
    }
}

fn web_search_lines(action: &serde_json::Value) -> Vec<(Option<char>, Line<'static>)> {
    let action_type = action["type"].as_str();
    let text = match action_type {
        Some("search") => action["query"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                action["queries"].as_array().map(|queries| {
                    queries
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
            })
            .map(|query| format!("search web: {query}"))
            .unwrap_or_else(|| "search web".to_owned()),
        Some("open_page") => action["url"]
            .as_str()
            .map(|url| format!("open page: {url}"))
            .unwrap_or_else(|| "open page".to_owned()),
        Some("find_in_page") => {
            let url = action["url"].as_str().unwrap_or_default();
            let pattern = action["pattern"].as_str().unwrap_or_default();
            format!("find in page: {pattern} ({url})")
        }
        _ => "search web".to_owned(),
    };
    vec![(Some('⌕'), Line::from(text))]
}

fn tool_call_lines(
    name: &str,
    arguments: Option<&serde_json::Value>,
) -> Vec<(Option<char>, Line<'static>)> {
    let object = arguments.and_then(serde_json::Value::as_object);
    match name {
        "runner" => runner_tool_lines(
            arguments
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
            &std::collections::BTreeMap::new(),
            None,
            false,
            false,
        ),
        "exec_command" => {
            let runner = object
                .and_then(|arguments| arguments.get("runner"))
                .and_then(serde_json::Value::as_str);
            let location = runner.unwrap_or(".").to_owned();
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
        "list_runners" => vec![(Some('◆'), Line::from("list runners"))],
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

fn runner_tool_lines(
    input: &str,
    results: &std::collections::BTreeMap<usize, RunnerResult>,
    pending_approval: Option<usize>,
    masked: bool,
    expanded: bool,
) -> Vec<(Option<char>, Line<'static>)> {
    let input = input.lines().collect::<Vec<_>>();
    let mut index = 0;
    let mut operation = 0;
    let mut lines = Vec::new();
    while index < input.len() {
        if let Some(runner) = input[index].strip_prefix("*** Runner ") {
            if operation > 0 {
                lines.push((None, Line::default()));
            }
            lines.push((Some('┌'), Line::from(runner.to_owned())));
            index += 1;
        } else if input[index] == "*** Command" {
            operation += 1;
            separate_operation(&mut lines, operation);
            lines.push(runner_operation_header(
                '◆',
                operation,
                "Command",
                pending_approval == Some(operation),
            ));
            index += 1;
            let end = input[index..]
                .iter()
                .position(|line| *line == "*** End")
                .map_or(input.len(), |offset| index + offset);
            lines.extend(
                bash_lines(&input[index..end].join("\n"))
                    .into_iter()
                    .enumerate()
                    .map(|(index, line)| ((index == 0).then_some('$'), line)),
            );
            index = (end + 1).min(input.len());
            append_runner_result(&mut lines, results.get(&operation), expanded);
        } else {
            index += 1;
        }
    }
    if masked {
        lines.push((
            Some('◇'),
            Line::from(Span::styled(
                "model context uses masked output",
                Style::default().fg(Color::DarkGray),
            )),
        ));
    }
    lines
}

fn runner_operation_header(
    marker: char,
    operation: usize,
    label: &str,
    approval_required: bool,
) -> (Option<char>, Line<'static>) {
    let text = if approval_required {
        format!("Operation {operation} · {label} · APPROVAL REQUIRED")
    } else {
        format!("Operation {operation} · {label}")
    };
    let style = if approval_required {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    (Some(marker), Line::from(Span::styled(text, style)))
}

fn append_runner_result(
    lines: &mut Vec<(Option<char>, Line<'static>)>,
    result: Option<&RunnerResult>,
    expanded: bool,
) {
    let Some(result) = result else {
        return;
    };
    let result_lines = match result {
        RunnerResult::Running {
            output,
            omitted_bytes,
        } => {
            let status = (
                Some('…'),
                Line::from(Span::styled(
                    "running",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
            );
            let mut result_lines = output
                .lines()
                .map(|line| {
                    (
                        None,
                        Line::from(Span::styled(
                            line.to_owned(),
                            Style::default().fg(Color::Gray),
                        )),
                    )
                })
                .collect::<Vec<_>>();
            if *omitted_bytes > 0 {
                result_lines.push((
                    None,
                    Line::from(Span::styled(
                        format!("… {omitted_bytes} output bytes omitted"),
                        Style::default().fg(Color::DarkGray),
                    )),
                ));
            }
            result_lines.push(status);
            result_lines
        }
        RunnerResult::Completed(ToolArtifact::RunnerOperation(operation)) => {
            runner_operation_result_lines(operation)
        }
        RunnerResult::Completed(
            ToolArtifact::CommandExecution(_) | ToolArtifact::PatchOperations(_),
        ) => Vec::new(),
    };
    lines.extend(fold_result_lines(result_lines, expanded));
}

fn fold_result_lines(
    lines: Vec<(Option<char>, Line<'static>)>,
    expanded: bool,
) -> Vec<(Option<char>, Line<'static>)> {
    const COLLAPSED_ROWS: usize = 12;
    const HEAD_ROWS: usize = 6;
    const TAIL_ROWS: usize = 5;

    if expanded || lines.len() <= COLLAPSED_ROWS {
        return lines;
    }
    let hidden = lines.len() - HEAD_ROWS - TAIL_ROWS;
    let mut folded = Vec::with_capacity(COLLAPSED_ROWS);
    folded.extend(lines.iter().take(HEAD_ROWS).cloned());
    folded.push((
        Some('…'),
        Line::from(Span::styled(
            format!("{hidden} rows hidden · Enter to expand"),
            Style::default().fg(Color::DarkGray),
        )),
    ));
    folded.extend(lines.into_iter().skip(HEAD_ROWS + hidden));
    folded
}

fn separate_operation(lines: &mut Vec<(Option<char>, Line<'static>)>, operation: usize) {
    if operation > 1 && !matches!(lines.last(), Some((Some('┌'), _))) {
        lines.push((None, Line::default()));
    }
}

fn bash_lines(command: &str) -> Vec<Line<'static>> {
    let syntax = SYNTAX_SET
        .find_syntax_by_token("bash")
        .expect("syntect includes bash syntax");
    let mut highlighter = HighlightLines::new(syntax, &THEME_SET.themes["base16-ocean.dark"]);
    let mut lines = LinesWithEndings::from(command)
        .map(|line| {
            let spans =
                highlighted_spans(&mut highlighter, line.trim_end_matches(['\r', '\n']), None)
                    .expect("bundled bash syntax is valid");
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

fn command_execution_lines(
    command: &CommandExecutionArtifact,
    status_last: bool,
) -> Vec<(Option<char>, Line<'static>)> {
    let (marker, label, style, output) = match command {
        CommandExecutionArtifact::Started { .. } => (
            '›',
            "started".to_owned(),
            Style::default().fg(Color::Cyan),
            "",
        ),
        CommandExecutionArtifact::Running { output, .. } => (
            '…',
            "running".to_owned(),
            Style::default().fg(Color::Yellow),
            output.as_str(),
        ),
        CommandExecutionArtifact::Finished {
            output, exit_code, ..
        } => {
            let success = *exit_code == Some(0);
            let exit_code = exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned());
            let style = if success {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Red)
            };
            (
                '✓',
                format!("finished · exit {exit_code}"),
                style,
                output.as_str(),
            )
        }
    };
    let status = (
        Some(marker),
        Line::from(Span::styled(label, style.add_modifier(Modifier::BOLD))),
    );
    let mut lines = output
        .lines()
        .map(|output| {
            (
                None,
                Line::from(Span::styled(
                    output.to_owned(),
                    Style::default().fg(Color::Gray),
                )),
            )
        })
        .collect::<Vec<_>>();
    if status_last {
        lines.push(status);
    } else {
        lines.insert(0, status);
    }
    lines
}

fn runner_operation_lines(data: &RunnerOperationArtifact) -> Vec<(Option<char>, Line<'static>)> {
    let mut lines = Vec::new();
    if data.operation > 1 {
        lines.push((None, Line::default()));
    }
    lines.push((
        Some('◆'),
        Line::from(Span::styled(
            format!(
                "Operation {} · {} · {}",
                data.operation, data.runner, data.label
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ));

    lines.extend(runner_operation_result_lines(data));
    lines
}

fn runner_operation_result_lines(
    data: &RunnerOperationArtifact,
) -> Vec<(Option<char>, Line<'static>)> {
    let mut lines = Vec::new();
    for artifact in &data.artifacts {
        let rendered = match artifact {
            ToolArtifact::CommandExecution(command) => command_execution_lines(command, true),
            ToolArtifact::PatchOperations(result) => patch_operation_lines(result),
            ToolArtifact::RunnerOperation(operation) => runner_operation_lines(operation),
        };
        lines.extend(rendered);
    }
    if data.artifacts.is_empty() {
        let result = data
            .result
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| data.result.to_string());
        lines.extend(result.lines().map(|line| {
            (
                None,
                Line::from(Span::styled(
                    line.to_owned(),
                    Style::default().fg(Color::Gray),
                )),
            )
        }));
    }
    lines
}

fn patch_operation_lines(result: &ApplyPatchResult) -> Vec<(Option<char>, Line<'static>)> {
    let results = match result {
        ApplyPatchResult::ParseError { error } => {
            let mut lines = vec![(
                Some('✗'),
                Line::from(Span::styled(
                    "patch parse failed",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
            )];
            lines.extend(
                error
                    .lines()
                    .map(|line| (None, Line::from(line.to_owned()))),
            );
            return lines;
        }
        ApplyPatchResult::Operations { results } => results,
    };
    let mut rendered = Vec::new();
    for result in results {
        let (label, outcome) = match result {
            PatchOperationResult::Added { path, outcome } => {
                (format!("A {}", path.display()), outcome)
            }
            PatchOperationResult::Deleted { path, outcome } => {
                (format!("D {}", path.display()), outcome)
            }
            PatchOperationResult::Updated { path, outcome } => {
                (format!("M {}", path.display()), outcome)
            }
            PatchOperationResult::Moved { from, to, outcome } => {
                (format!("R {} → {}", from.display(), to.display()), outcome)
            }
        };
        match outcome {
            PatchOperationOutcome::Applied { diff: Ok(diff) } => {
                rendered.extend(file_diff_lines(diff));
            }
            PatchOperationOutcome::Applied { diff: Err(_) } => rendered.push((
                Some('✓'),
                Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
            )),
            PatchOperationOutcome::Failed { error } => {
                rendered.push((
                    Some('✗'),
                    Line::from(Span::styled(
                        label,
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )),
                ));
                rendered.extend(
                    error
                        .lines()
                        .map(|line| (None, Line::from(line.to_owned()))),
                );
            }
        }
    }
    rendered
}

fn file_diff_lines(diff: &FileDiff) -> Vec<(Option<char>, Line<'static>)> {
    let line_number_width = diff
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .flat_map(|line| [line.old_line, line.new_line])
        .flatten()
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(1);
    let mut rendered = Vec::new();
    let label = match (&diff.old_path, &diff.new_path) {
        (None, Some(path)) => format!("A {}", path.display()),
        (Some(path), None) => format!("D {}", path.display()),
        (Some(old), Some(new)) if old == new => format!("M {}", old.display()),
        (Some(old), Some(new)) => format!("R {} → {}", old.display(), new.display()),
        (None, None) => unreachable!(),
    };
    rendered.push((
        Some('±'),
        Line::from(Span::styled(
            label,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ));
    for hunk in &diff.hunks {
        let syntax = diff
            .new_path
            .as_ref()
            .or(diff.old_path.as_ref())
            .and_then(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| SYNTAX_SET.find_syntax_by_extension(name))
                    .or_else(|| {
                        path.extension()
                            .and_then(|extension| extension.to_str())
                            .and_then(|extension| SYNTAX_SET.find_syntax_by_extension(extension))
                    })
            });
        let mut old_highlighter = syntax
            .map(|syntax| HighlightLines::new(syntax, &THEME_SET.themes["base16-ocean.dark"]));
        let mut new_highlighter = syntax
            .map(|syntax| HighlightLines::new(syntax, &THEME_SET.themes["base16-ocean.dark"]));
        rendered.push((
            None,
            Line::from(Span::styled(
                format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
                ),
                Style::default().fg(Color::Magenta),
            )),
        ));
        for line in &hunk.lines {
            let old_line = line
                .old_line
                .map(|line| line.to_string())
                .unwrap_or_default();
            let new_line = line
                .new_line
                .map(|line| line.to_string())
                .unwrap_or_default();
            let gutter = format!("{old_line:>line_number_width$} {new_line:>line_number_width$} ");
            let (sign, diff_style, code_spans) = match line.kind {
                DiffLineKind::Context => {
                    if let Some(highlighter) = old_highlighter.as_mut() {
                        let _ = highlighted_spans(highlighter, &line.text, None);
                    }
                    let spans = new_highlighter
                        .as_mut()
                        .and_then(|highlighter| highlighted_spans(highlighter, &line.text, None));
                    (' ', Style::default(), spans)
                }
                DiffLineKind::Added => {
                    let background = Color::Rgb(20, 45, 35);
                    let spans = new_highlighter.as_mut().and_then(|highlighter| {
                        highlighted_spans(highlighter, &line.text, Some(background))
                    });
                    ('+', Style::default().fg(Color::Green).bg(background), spans)
                }
                DiffLineKind::Removed => {
                    let background = Color::Rgb(50, 25, 30);
                    let spans = old_highlighter.as_mut().and_then(|highlighter| {
                        highlighted_spans(highlighter, &line.text, Some(background))
                    });
                    ('-', Style::default().fg(Color::Red).bg(background), spans)
                }
            };
            let code_spans =
                code_spans.unwrap_or_else(|| vec![Span::styled(line.text.clone(), diff_style)]);
            let mut spans = vec![
                Span::styled(gutter, Style::default().fg(Color::DarkGray)),
                Span::styled(sign.to_string(), diff_style),
            ];
            spans.extend(code_spans);
            rendered.push((None, Line::from(spans)));
        }
    }
    rendered
}

fn highlighted_spans(
    highlighter: &mut HighlightLines<'_>,
    text: &str,
    background: Option<Color>,
) -> Option<Vec<Span<'static>>> {
    let line = format!("{text}\n");
    Some(
        highlighter
            .highlight_line(&line, &SYNTAX_SET)
            .ok()?
            .into_iter()
            .filter_map(|(style, text)| {
                let text = text.trim_end_matches(['\r', '\n']);
                (!text.is_empty()).then(|| {
                    let mut ratatui_style = Style::default().fg(Color::Rgb(
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                    ));
                    if let Some(background) = background {
                        ratatui_style = ratatui_style.bg(background);
                    }
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
            .collect(),
    )
}

fn format_tool_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        value => serde_json::to_string_pretty(value).unwrap(),
    }
}
