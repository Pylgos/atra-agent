use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use icu_segmenter::WordSegmenterBorrowed;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) struct InputBuffer {
    pub(crate) value: String,
    pub(crate) cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    multiline: bool,
}

pub(super) enum InputAction {
    None,
    Submit,
}

impl InputBuffer {
    pub(super) fn new(history: Vec<String>, multiline: bool) -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            history,
            history_index: None,
            history_draft: String::new(),
            multiline,
        }
    }

    pub(super) fn handle_key(
        &mut self,
        key: KeyEvent,
        word_segmenter: &WordSegmenterBorrowed<'static>,
    ) -> InputAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('a') => self.cursor = 0,
                KeyCode::Char('e') => self.cursor = self.value.len(),
                KeyCode::Char('u') => {
                    self.value.drain(..self.cursor);
                    self.cursor = 0;
                    self.reset_history_navigation();
                }
                KeyCode::Char('k') => {
                    self.value.truncate(self.cursor);
                    self.reset_history_navigation();
                }
                KeyCode::Char('c') => {
                    self.value.clear();
                    self.cursor = 0;
                    self.reset_history_navigation();
                }
                KeyCode::Char('w') | KeyCode::Backspace => {
                    self.delete_word_backward(word_segmenter)
                }
                KeyCode::Left => self.move_word_backward(word_segmenter),
                KeyCode::Right => self.move_word_forward(word_segmenter),
                _ => {}
            }
            return InputAction::None;
        }

        match key.code {
            KeyCode::Enter if !self.multiline => return InputAction::Submit,
            KeyCode::Enter => self.insert('\n'),
            KeyCode::Backspace => self.delete_backward(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left => self.move_backward(),
            KeyCode::Right => self.move_forward(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            KeyCode::Up if self.multiline => self.move_up_or_previous_history(),
            KeyCode::Down if self.multiline => self.move_down_or_next_history(),
            KeyCode::Up => self.previous_history(),
            KeyCode::Down => self.next_history(),
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert(character)
            }
            _ => {}
        }
        InputAction::None
    }

    pub(super) fn set(&mut self, value: String) {
        self.value = value;
        self.cursor = self.value.len();
        self.reset_history_navigation();
    }

    pub(super) fn take(&mut self) -> String {
        self.cursor = 0;
        self.reset_history_navigation();
        std::mem::take(&mut self.value)
    }

    pub(super) fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.reset_history_navigation();
    }

    pub(super) fn record_history(&mut self, value: String) {
        self.history.push(value);
        self.reset_history_navigation();
    }

    pub(super) fn reset_history_navigation(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
    }

    fn insert(&mut self, character: char) {
        self.value.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.reset_history_navigation();
    }

    fn delete_backward(&mut self) {
        if let Some((index, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.value.drain(index..self.cursor);
            self.cursor = index;
            self.reset_history_navigation();
        }
    }

    fn delete_forward(&mut self) {
        if let Some(character) = self.value[self.cursor..].chars().next() {
            self.value
                .drain(self.cursor..self.cursor + character.len_utf8());
            self.reset_history_navigation();
        }
    }

    fn move_backward(&mut self) {
        if let Some((index, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn move_forward(&mut self) {
        if let Some(character) = self.value[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn move_up_or_previous_history(&mut self) {
        let current_line_start = self.value[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        if current_line_start == 0 {
            self.previous_history();
            return;
        }

        let column = self.value[current_line_start..self.cursor].width();
        let previous_line_end = current_line_start - 1;
        let previous_line_start = self.value[..previous_line_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.cursor = column_offset(&self.value, previous_line_start, previous_line_end, column);
    }

    fn move_down_or_next_history(&mut self) {
        let current_line_end = self.value[self.cursor..]
            .find('\n')
            .map(|offset| self.cursor + offset);
        let Some(current_line_end) = current_line_end else {
            self.next_history();
            return;
        };

        let current_line_start = self.value[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let column = self.value[current_line_start..self.cursor].width();
        let next_line_start = current_line_end + 1;
        let next_line_end = self.value[next_line_start..]
            .find('\n')
            .map_or(self.value.len(), |offset| next_line_start + offset);
        self.cursor = column_offset(&self.value, next_line_start, next_line_end, column);
    }

    fn delete_word_backward(&mut self, word_segmenter: &WordSegmenterBorrowed<'static>) {
        let end = self.cursor;
        self.move_word_backward(word_segmenter);
        if self.cursor < end {
            self.value.drain(self.cursor..end);
            self.reset_history_navigation();
        }
    }

    fn move_word_backward(&mut self, word_segmenter: &WordSegmenterBorrowed<'static>) {
        let mut start = 0;
        let mut previous_word = None;
        for (end, word_type) in word_segmenter
            .segment_str(&self.value)
            .iter_with_word_type()
        {
            if word_type.is_word_like() && start < self.cursor {
                previous_word = Some(start);
            }
            if end >= self.cursor {
                break;
            }
            start = end;
        }
        self.cursor = previous_word.unwrap_or(0);
    }

    fn move_word_forward(&mut self, word_segmenter: &WordSegmenterBorrowed<'static>) {
        for (end, word_type) in word_segmenter
            .segment_str(&self.value)
            .iter_with_word_type()
        {
            if word_type.is_word_like() && end > self.cursor {
                self.cursor = end;
                return;
            }
        }
        self.cursor = self.value.len();
    }

    fn previous_history(&mut self) {
        let index = match self.history_index {
            Some(0) => return,
            Some(index) => index - 1,
            None if self.history.is_empty() => return,
            None => {
                self.history_draft.clone_from(&self.value);
                self.history.len() - 1
            }
        };
        self.history_index = Some(index);
        self.value.clone_from(&self.history[index]);
        self.cursor = self.value.len();
    }

    fn next_history(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.value.clone_from(&self.history[index + 1]);
        } else {
            self.history_index = None;
            self.value = std::mem::take(&mut self.history_draft);
        }
        self.cursor = self.value.len();
    }
}

fn column_offset(value: &str, start: usize, end: usize, column: usize) -> usize {
    let mut width = 0;
    for (offset, character) in value[start..end].char_indices() {
        let next_width = width + character.width().unwrap_or(0);
        if next_width > column {
            return start + offset;
        }
        width = next_width;
        if width == column {
            return start + offset + character.len_utf8();
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::InputBuffer;

    #[test]
    fn multiline_arrows_move_between_lines_before_navigating_history() {
        let mut input = InputBuffer::new(vec!["history".to_owned()], true);
        input.set("first\nsecond\nthird".to_owned());
        input.cursor = "first\nsec".len();

        input.move_up_or_previous_history();
        assert_eq!(input.value, "first\nsecond\nthird");
        assert_eq!(input.cursor, "fir".len());

        input.move_down_or_next_history();
        assert_eq!(input.cursor, "first\nsec".len());
    }

    #[test]
    fn multiline_arrows_navigate_history_only_at_vertical_boundaries() {
        let mut input = InputBuffer::new(vec!["history".to_owned()], true);
        input.set("first\nsecond".to_owned());
        input.cursor = 2;

        input.move_up_or_previous_history();
        assert_eq!(input.value, "history");
        assert_eq!(input.cursor, input.value.len());

        input.move_down_or_next_history();
        assert_eq!(input.value, "first\nsecond");
        assert_eq!(input.cursor, input.value.len());
    }

    #[test]
    fn vertical_movement_clamps_to_shorter_lines() {
        let mut input = InputBuffer::new(Vec::new(), true);
        input.set("long line\n短\nlast".to_owned());
        input.cursor = "long li".len();

        input.move_down_or_next_history();
        assert_eq!(input.cursor, "long line\n短".len());

        input.move_down_or_next_history();
        assert_eq!(input.cursor, "long line\n短\nla".len());
    }
}
