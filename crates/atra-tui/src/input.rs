use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use icu_segmenter::WordSegmenterBorrowed;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) struct InputBuffer {
    pub(crate) value: String,
    pub(crate) cursor: usize,
    selection_anchor: Option<usize>,
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
            selection_anchor: None,
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
        let selecting = key.modifiers.contains(KeyModifiers::SHIFT);
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('a') => self.move_to(0, selecting),
                KeyCode::Char('e') => self.move_to(self.value.len(), selecting),
                KeyCode::Char('u') => {
                    if !self.delete_selection() {
                        self.value.drain(..self.cursor);
                        self.cursor = 0;
                        self.reset_history_navigation();
                    }
                }
                KeyCode::Char('k') => {
                    if !self.delete_selection() {
                        self.value.truncate(self.cursor);
                        self.reset_history_navigation();
                    }
                }
                KeyCode::Char('c') => {
                    self.value.clear();
                    self.cursor = 0;
                    self.selection_anchor = None;
                    self.reset_history_navigation();
                }
                KeyCode::Char('w') | KeyCode::Backspace => {
                    self.delete_word_backward(word_segmenter)
                }
                KeyCode::Left => self.move_word_backward(word_segmenter, selecting),
                KeyCode::Right => self.move_word_forward(word_segmenter, selecting),
                _ => {}
            }
            return InputAction::None;
        }

        match key.code {
            KeyCode::Enter if !self.multiline => return InputAction::Submit,
            KeyCode::Enter => self.insert('\n'),
            KeyCode::Backspace => self.delete_backward(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left => self.move_backward(selecting),
            KeyCode::Right => self.move_forward(selecting),
            KeyCode::Home => self.move_to(0, selecting),
            KeyCode::End => self.move_to(self.value.len(), selecting),
            KeyCode::Up if self.multiline && selecting => {
                self.move_up(true);
            }
            KeyCode::Down if self.multiline && selecting => {
                self.move_down(true);
            }
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
        self.selection_anchor = None;
        self.reset_history_navigation();
    }

    pub(super) fn take(&mut self) -> String {
        self.cursor = 0;
        self.selection_anchor = None;
        self.reset_history_navigation();
        std::mem::take(&mut self.value)
    }

    pub(super) fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.selection_anchor = None;
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
        self.delete_selection();
        self.value.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.reset_history_navigation();
    }

    fn delete_backward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some((index, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.value.drain(index..self.cursor);
            self.cursor = index;
            self.reset_history_navigation();
        }
    }

    fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(character) = self.value[self.cursor..].chars().next() {
            self.value
                .drain(self.cursor..self.cursor + character.len_utf8());
            self.reset_history_navigation();
        }
    }

    fn move_backward(&mut self, selecting: bool) {
        if !selecting && let Some((start, _)) = self.selection_range() {
            self.move_to(start, false);
            return;
        }
        let previous = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(self.cursor, |(index, _)| index);
        self.move_to(previous, selecting);
    }

    fn move_forward(&mut self, selecting: bool) {
        if !selecting && let Some((_, end)) = self.selection_range() {
            self.move_to(end, false);
            return;
        }
        let next = self.value[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |character| self.cursor + character.len_utf8());
        self.move_to(next, selecting);
    }

    fn move_to(&mut self, cursor: usize, selecting: bool) {
        if selecting {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        self.cursor = cursor;
    }

    fn move_up(&mut self, selecting: bool) -> bool {
        let current_line_start = self.value[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        if current_line_start == 0 {
            return false;
        }
        let column = self.value[current_line_start..self.cursor].width();
        let previous_line_end = current_line_start - 1;
        let previous_line_start = self.value[..previous_line_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let cursor = column_offset(&self.value, previous_line_start, previous_line_end, column);
        self.move_to(cursor, selecting);
        true
    }

    fn move_down(&mut self, selecting: bool) -> bool {
        let current_line_end = self.value[self.cursor..]
            .find('\n')
            .map(|offset| self.cursor + offset);
        let Some(current_line_end) = current_line_end else {
            return false;
        };
        let current_line_start = self.value[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let column = self.value[current_line_start..self.cursor].width();
        let next_line_start = current_line_end + 1;
        let next_line_end = self.value[next_line_start..]
            .find('\n')
            .map_or(self.value.len(), |offset| next_line_start + offset);
        let cursor = column_offset(&self.value, next_line_start, next_line_end, column);
        self.move_to(cursor, selecting);
        true
    }

    pub(super) fn begin_selection(&mut self, cursor: usize) {
        let cursor = self.valid_cursor(cursor);
        self.cursor = cursor;
        self.selection_anchor = Some(cursor);
        self.reset_history_navigation();
    }

    pub(super) fn extend_selection(&mut self, cursor: usize) {
        self.cursor = self.valid_cursor(cursor);
    }

    pub(super) fn is_selecting(&self) -> bool {
        self.selection_anchor.is_some()
    }

    pub(crate) fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        (anchor != self.cursor).then_some({
            if anchor < self.cursor {
                (anchor, self.cursor)
            } else {
                (self.cursor, anchor)
            }
        })
    }

    fn valid_cursor(&self, cursor: usize) -> usize {
        let mut cursor = cursor.min(self.value.len());
        while !self.value.is_char_boundary(cursor) {
            cursor -= 1;
        }
        cursor
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection_range() else {
            self.selection_anchor = None;
            return false;
        };
        self.value.drain(start..end);
        self.cursor = start;
        self.selection_anchor = None;
        self.reset_history_navigation();
        true
    }

    fn move_up_or_previous_history(&mut self) {
        self.selection_anchor = None;
        if !self.move_up(false) {
            self.previous_history();
        }
    }

    fn move_down_or_next_history(&mut self) {
        self.selection_anchor = None;
        if !self.move_down(false) {
            self.next_history();
        };
    }

    fn delete_word_backward(&mut self, word_segmenter: &WordSegmenterBorrowed<'static>) {
        if self.delete_selection() {
            return;
        }
        let end = self.cursor;
        self.move_word_backward(word_segmenter, false);
        if self.cursor < end {
            self.value.drain(self.cursor..end);
            self.reset_history_navigation();
        }
    }

    fn move_word_backward(
        &mut self,
        word_segmenter: &WordSegmenterBorrowed<'static>,
        selecting: bool,
    ) {
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
        self.move_to(previous_word.unwrap_or(0), selecting);
    }

    fn move_word_forward(
        &mut self,
        word_segmenter: &WordSegmenterBorrowed<'static>,
        selecting: bool,
    ) {
        for (end, word_type) in word_segmenter
            .segment_str(&self.value)
            .iter_with_word_type()
        {
            if word_type.is_word_like() && end > self.cursor {
                self.move_to(end, selecting);
                return;
            }
        }
        self.move_to(self.value.len(), selecting);
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
        self.selection_anchor = None;
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
        self.selection_anchor = None;
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use icu_segmenter::{WordSegmenter, options::WordBreakInvariantOptions};

    fn handle(input: &mut InputBuffer, code: KeyCode, modifiers: KeyModifiers) {
        let segmenter = WordSegmenter::new_auto(WordBreakInvariantOptions::default());
        input.handle_key(KeyEvent::new(code, modifiers), &segmenter);
    }

    #[test]
    fn shift_movement_selects_and_backspace_deletes_the_range() {
        let mut input = InputBuffer::new(Vec::new(), true);
        input.set("a日本b".to_owned());

        handle(&mut input, KeyCode::Left, KeyModifiers::SHIFT);
        handle(&mut input, KeyCode::Left, KeyModifiers::SHIFT);
        assert_eq!(input.selection_range(), Some(("a日".len(), "a日本b".len())));

        handle(&mut input, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(input.value, "a日");
        assert_eq!(input.cursor, input.value.len());
        assert_eq!(input.selection_range(), None);
    }

    #[test]
    fn delete_removes_selection_or_the_character_after_the_cursor() {
        let mut input = InputBuffer::new(Vec::new(), false);
        input.set("a日本b".to_owned());
        input.cursor = 1;

        handle(&mut input, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(input.value, "a本b");
        assert_eq!(input.cursor, 1);

        input.begin_selection(1);
        input.extend_selection("a本".len());
        handle(&mut input, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(input.value, "ab");
        assert_eq!(input.cursor, 1);
    }

    #[test]
    fn typing_replaces_a_selection_in_single_line_command_input() {
        let mut input = InputBuffer::new(Vec::new(), false);
        input.set("checkpoint".to_owned());
        input.begin_selection("check".len());
        input.extend_selection(input.value.len());

        handle(&mut input, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(input.value, "checkx");
        assert_eq!(input.cursor, input.value.len());
    }

    #[test]
    fn plain_arrow_collapses_a_selection() {
        let mut input = InputBuffer::new(Vec::new(), false);
        input.set("abcd".to_owned());
        input.begin_selection(1);
        input.extend_selection(3);

        handle(&mut input, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(input.cursor, 1);
        assert_eq!(input.selection_range(), None);

        input.begin_selection(1);
        input.extend_selection(3);
        handle(&mut input, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(input.cursor, 3);
        assert_eq!(input.selection_range(), None);
    }

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
