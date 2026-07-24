use crate::app::App;

impl App {
    pub(super) fn insert(&mut self, character: char) {
        self.input.insert(self.input_cursor, character);
        self.input_cursor += character.len_utf8();
        self.reset_history_navigation();
    }

    pub(super) fn delete_backward(&mut self) {
        if let Some((index, _)) = self.input[..self.input_cursor].char_indices().next_back() {
            self.input.drain(index..self.input_cursor);
            self.input_cursor = index;
            self.reset_history_navigation();
        }
    }

    pub(super) fn delete_forward(&mut self) {
        if let Some(character) = self.input[self.input_cursor..].chars().next() {
            self.input
                .drain(self.input_cursor..self.input_cursor + character.len_utf8());
            self.reset_history_navigation();
        }
    }

    pub(super) fn move_backward(&mut self) {
        if let Some((index, _)) = self.input[..self.input_cursor].char_indices().next_back() {
            self.input_cursor = index;
        }
    }

    pub(super) fn move_forward(&mut self) {
        if let Some(character) = self.input[self.input_cursor..].chars().next() {
            self.input_cursor += character.len_utf8();
        }
    }

    pub(super) fn delete_word_backward(&mut self) {
        let end = self.input_cursor;
        self.move_word_backward();
        if self.input_cursor < end {
            self.input.drain(self.input_cursor..end);
            self.reset_history_navigation();
        }
    }

    pub(super) fn move_word_backward(&mut self) {
        let mut start = 0;
        let mut previous_word = None;
        for (end, word_type) in self
            .word_segmenter
            .segment_str(&self.input)
            .iter_with_word_type()
        {
            if word_type.is_word_like() && start < self.input_cursor {
                previous_word = Some(start);
            }
            if end >= self.input_cursor {
                break;
            }
            start = end;
        }
        self.input_cursor = previous_word.unwrap_or(0);
    }

    pub(super) fn move_word_forward(&mut self) {
        for (end, word_type) in self
            .word_segmenter
            .segment_str(&self.input)
            .iter_with_word_type()
        {
            if word_type.is_word_like() && end > self.input_cursor {
                self.input_cursor = end;
                return;
            }
        }
        self.input_cursor = self.input.len();
    }

    pub(super) fn previous_history(&mut self) {
        let index = match self.history_index {
            Some(0) => return,
            Some(index) => index - 1,
            None if self.input_history.is_empty() => return,
            None => {
                self.history_draft.clone_from(&self.input);
                self.input_history.len() - 1
            }
        };
        self.history_index = Some(index);
        self.input.clone_from(&self.input_history[index]);
        self.input_cursor = self.input.len();
    }

    pub(super) fn next_history(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.input_history.len() {
            self.history_index = Some(index + 1);
            self.input.clone_from(&self.input_history[index + 1]);
        } else {
            self.history_index = None;
            self.input = std::mem::take(&mut self.history_draft);
        }
        self.input_cursor = self.input.len();
    }

    pub(super) fn reset_history_navigation(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
    }
}
