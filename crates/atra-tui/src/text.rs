use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

const TAB_WIDTH: usize = 4;

pub(crate) fn expand_tabs(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut column = 0;
    for character in input.chars() {
        match character {
            '\t' => {
                let spaces = TAB_WIDTH - column % TAB_WIDTH;
                output.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            '\n' => {
                output.push(character);
                column = 0;
            }
            _ => {
                output.push(character);
                column += character.width().unwrap_or(0);
            }
        }
    }
    output
}

pub(crate) fn expand_line_tabs(line: Line<'static>) -> Line<'static> {
    let mut column = 0;
    let spans = line
        .spans
        .into_iter()
        .map(|span| {
            let mut content = String::with_capacity(span.content.len());
            for character in span.content.chars() {
                if character == '\t' {
                    let spaces = TAB_WIDTH - column % TAB_WIDTH;
                    content.extend(std::iter::repeat_n(' ', spaces));
                    column += spaces;
                } else {
                    content.push(character);
                    column += character.width().unwrap_or(0);
                }
            }
            Span::styled(content, span.style)
        })
        .collect();
    Line {
        style: line.style,
        alignment: line.alignment,
        spans,
    }
}
