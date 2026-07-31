use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

const TAB_WIDTH: usize = 4;

pub(crate) fn offset_at_position(input: &str, row: usize, column: usize) -> usize {
    let mut line_start = 0;
    for _ in 0..row {
        let Some(newline) = input[line_start..].find('\n') else {
            return input.len();
        };
        line_start += newline + 1;
    }

    let line_end = input[line_start..]
        .find('\n')
        .map_or(input.len(), |newline| line_start + newline);
    let mut current_column = 0;
    for (offset, character) in input[line_start..line_end].char_indices() {
        let width = if character == '\t' {
            TAB_WIDTH - current_column % TAB_WIDTH
        } else {
            character.width().unwrap_or(0)
        };
        if column < current_column + width {
            return line_start + offset;
        }
        current_column += width;
    }
    line_end
}

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

#[cfg(test)]
mod tests {
    use super::offset_at_position;

    #[test]
    fn maps_display_positions_across_unicode_tabs_and_lines() {
        let value = "a\t日\nxyz";

        assert_eq!(offset_at_position(value, 0, 0), 0);
        assert_eq!(offset_at_position(value, 0, 1), 1);
        assert_eq!(offset_at_position(value, 0, 3), 1);
        assert_eq!(offset_at_position(value, 0, 4), 2);
        assert_eq!(offset_at_position(value, 0, 6), "a\t日".len());
        assert_eq!(offset_at_position(value, 1, 2), "a\t日\nxy".len());
        assert_eq!(offset_at_position(value, 5, 0), value.len());
    }
}
