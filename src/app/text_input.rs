//! A prompt's text and the cursor inside it, plus the wrap-around cursor into
//! a prompt's result list.
//!
//! Every prompt shares this one implementation, so no key handler holds editing
//! logic of its own that could drift (E7/F13). The text cursor is a *character*
//! index, never a byte offset: the open-workspace prompt is pre-filled with the
//! working directory, and a path holding one multi-byte character would
//! otherwise split a `String` mid-character on the first edit.

use super::{App, Prompt};

/// A wrap-around cursor into a prompt's result list (F13).
///
/// The same nine-line body — clamp an empty list to zero, wrap a negative step
/// backwards, take a positive step modulo the length — was written out once per
/// prompt, each time inside a re-match of the prompt for borrow reasons. There
/// is one copy now, and the prompts hold it instead of a bare `usize`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListSelection {
    index: usize,
}

impl ListSelection {
    pub fn index(self) -> usize {
        self.index
    }

    /// Move by `delta` within a list of `len` entries, wrapping at both ends.
    /// An empty list has nothing to select, so it resets to zero.
    ///
    /// A true modular wrap in both directions, for any `delta` (F21). The
    /// backwards branch used to be `checked_sub(delta).unwrap_or(len - delta)`,
    /// which agrees with a wrap only for a step of one: at `len = 5, index = 1,
    /// delta = -3` it landed on `2` where wrapping gives `3`. Every caller steps
    /// by exactly ±1, so nothing was ever wrong on screen — but a page-sized
    /// step is the obvious next binding to add, and it would have been.
    ///
    /// Both magnitudes are reduced modulo `len` before anything is added, so the
    /// arithmetic cannot overflow whatever `delta` and `index` are.
    pub fn step(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.index = 0;
            return;
        }
        let index = self.index % len;
        let step = delta.unsigned_abs() % len;
        self.index = if delta.is_negative() {
            (index + len - step) % len
        } else {
            (index + step) % len
        };
    }

    /// Keep the selection inside a list that has just changed length — what a
    /// keystroke does to the palette's filtered entries on every character.
    pub fn clamp(&mut self, len: usize) {
        self.index = self.index.min(len.saturating_sub(1));
    }

    pub fn reset(&mut self) {
        self.index = 0;
    }
}

/// A prompt's text together with the cursor inside it (E7).
///
/// Every prompt shares this one implementation, so the four key handlers hold no
/// editing logic of their own that could drift apart (and Slice 10 can collapse
/// them without touching any of it). The cursor is a *character* index, never a
/// byte offset: the open-workspace prompt is pre-filled with the working
/// directory, and a path holding one multi-byte character would otherwise split
/// a `String` mid-character on the first edit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptInput {
    text: String,
    /// Characters between the start of `text` and the cursor, `0..=chars`.
    cursor: usize,
}

impl PromptInput {
    /// Pre-filled input with the cursor at the end, where typing continues.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        Self { text, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Cursor position in characters from the start of the input.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Text left of the cursor. Rendering measures its *display* width, so a
    /// wide character before the cursor moves the cursor two columns.
    pub fn before_cursor(&self) -> &str {
        &self.text[..self.byte_offset(self.cursor)]
    }

    /// The character the cursor sits on, or `None` at the end of the input.
    pub fn char_at_cursor(&self) -> Option<char> {
        self.text[self.byte_offset(self.cursor)..].chars().next()
    }

    /// Text right of the cursor cell (excluding the character under it).
    pub fn after_cursor(&self) -> &str {
        let start = self.byte_offset(self.cursor);
        match self.text[start..].chars().next() {
            Some(ch) => &self.text[start + ch.len_utf8()..],
            None => "",
        }
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, ch: char) {
        let offset = self.byte_offset(self.cursor);
        self.text.insert(offset, ch);
        self.cursor += 1;
    }

    /// Backspace. Returns whether anything was removed.
    pub fn delete_before_cursor(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.replace_range(self.cursor - 1, self.cursor);
        true
    }

    /// Forward delete (the `Delete` key). Returns whether anything was removed.
    pub fn delete_at_cursor(&mut self) -> bool {
        if self.cursor >= self.char_count() {
            return false;
        }
        self.replace_range(self.cursor, self.cursor + 1);
        true
    }

    /// Ctrl+w: the whitespace-delimited word before the cursor, plus any
    /// whitespace between it and the cursor.
    pub fn delete_word_before_cursor(&mut self) -> bool {
        let chars = self.text.chars().collect::<Vec<_>>();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        if start == self.cursor {
            return false;
        }
        self.replace_range(start, self.cursor);
        true
    }

    /// Ctrl+u: everything left of the cursor.
    pub fn delete_to_start(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.replace_range(0, self.cursor);
        true
    }

    /// Ctrl+k: everything from the cursor to the end.
    pub fn delete_to_end(&mut self) -> bool {
        let end = self.char_count();
        if self.cursor >= end {
            return false;
        }
        self.replace_range(self.cursor, end);
        true
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    pub fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    pub fn move_to_end(&mut self) {
        self.cursor = self.char_count();
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// Byte offset of character index `cursor`, clamped to the end of the text.
    fn byte_offset(&self, cursor: usize) -> usize {
        self.text
            .char_indices()
            .nth(cursor)
            .map(|(offset, _)| offset)
            .unwrap_or(self.text.len())
    }

    /// Remove characters `start..end` and leave the cursor where the removed
    /// text began — the single place that touches byte offsets.
    fn replace_range(&mut self, start: usize, end: usize) {
        let (start_offset, end_offset) = (self.byte_offset(start), self.byte_offset(end));
        self.text.replace_range(start_offset..end_offset, "");
        self.cursor = start;
    }
}

impl std::ops::Deref for PromptInput {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl App {
    /// The text of the open prompt, if it has one. Every editing entry point
    /// below goes through here, so cursor handling exists once (E7).
    fn prompt_input_mut(&mut self) -> Option<&mut PromptInput> {
        match self.mode.prompt_mut() {
            Some(Prompt::OpenWorkspace(prompt)) => Some(&mut prompt.input),
            Some(Prompt::NewTerminalCommand(prompt)) => Some(&mut prompt.input),
            Some(Prompt::CommandPalette(prompt)) => Some(&mut prompt.input),
            Some(Prompt::Search(prompt)) => Some(&mut prompt.input),
            Some(Prompt::ConfirmDelete(_))
            | Some(Prompt::ConfirmRestore(_))
            | Some(Prompt::Help(_))
            | None => None,
        }
    }

    /// Apply `edit` to the open prompt's text, then re-run everything that keys
    /// off the *whole* input: the stale error goes, and the filtered lists are
    /// re-selected from the top. Matching reads the full text, never the part
    /// before the cursor, so editing mid-string filters on what is on screen.
    fn edit_prompt_input(&mut self, edit: impl FnOnce(&mut PromptInput)) {
        let Some(input) = self.prompt_input_mut() else {
            return;
        };
        edit(input);

        match self.mode.prompt_mut() {
            Some(Prompt::OpenWorkspace(prompt)) => {
                prompt.error = None;
                prompt.selected.reset();
            }
            Some(Prompt::NewTerminalCommand(prompt)) => prompt.error = None,
            Some(Prompt::CommandPalette(prompt)) => prompt.selected.reset(),
            Some(Prompt::Search(prompt)) => prompt.error = None,
            _ => {}
        }
        self.clamp_command_palette_selection();
    }

    /// Move the cursor without touching the text: no error is cleared and no
    /// list selection is reset, because nothing the list depends on changed.
    fn move_prompt_cursor(&mut self, move_cursor: impl FnOnce(&mut PromptInput)) {
        if let Some(input) = self.prompt_input_mut() {
            move_cursor(input);
        }
    }

    pub fn push_prompt_char(&mut self, c: char) {
        self.edit_prompt_input(|input| input.insert(c));
    }

    pub fn pop_prompt_char(&mut self) {
        self.edit_prompt_input(|input| {
            input.delete_before_cursor();
        });
    }

    pub fn delete_prompt_char_at_cursor(&mut self) {
        self.edit_prompt_input(|input| {
            input.delete_at_cursor();
        });
    }

    pub fn delete_prompt_word_before_cursor(&mut self) {
        self.edit_prompt_input(|input| {
            input.delete_word_before_cursor();
        });
    }

    pub fn delete_prompt_to_start(&mut self) {
        self.edit_prompt_input(|input| {
            input.delete_to_start();
        });
    }

    pub fn delete_prompt_to_end(&mut self) {
        self.edit_prompt_input(|input| {
            input.delete_to_end();
        });
    }

    pub fn move_prompt_cursor_left(&mut self) {
        self.move_prompt_cursor(PromptInput::move_left);
    }

    pub fn move_prompt_cursor_right(&mut self) {
        self.move_prompt_cursor(PromptInput::move_right);
    }

    pub fn move_prompt_cursor_to_start(&mut self) {
        self.move_prompt_cursor(PromptInput::move_to_start);
    }

    pub fn move_prompt_cursor_to_end(&mut self) {
        self.move_prompt_cursor(PromptInput::move_to_end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_editing_works_from_the_middle_of_the_input() {
        let mut app = App::two_workspaces();
        app.begin_open_workspace(&[]);
        if let Some(Prompt::OpenWorkspace(prompt)) = app.prompt_mut() {
            prompt.input = PromptInput::new("/home/user/prjects/mult");
        }

        // Fix a typo in the middle without retyping the tail (E7).
        for _ in 0.."jects/mult".chars().count() {
            app.move_prompt_cursor_left();
        }
        app.push_prompt_char('o');

        let Some(Prompt::OpenWorkspace(prompt)) = app.prompt() else {
            panic!("prompt stays open");
        };
        assert_eq!(prompt.input.as_str(), "/home/user/projects/mult");
        assert_eq!(prompt.input.cursor(), "/home/user/pro".chars().count());
    }

    #[test]
    fn prompt_editing_never_splits_a_multi_byte_character() {
        let mut input = PromptInput::new("café 漢字 dir");

        input.move_to_start();
        input.move_right();
        input.move_right();
        input.move_right();
        // The cursor sits on `é`; forward delete removes the whole character.
        assert_eq!(input.char_at_cursor(), Some('é'));
        assert!(input.delete_at_cursor());
        assert_eq!(input.as_str(), "caf 漢字 dir");

        input.move_to_end();
        assert!(input.delete_word_before_cursor());
        assert_eq!(input.as_str(), "caf 漢字 ");
        assert!(input.delete_word_before_cursor());
        assert_eq!(input.as_str(), "caf ");
        assert_eq!(input.cursor(), 4);

        // Splitting for rendering is on character boundaries too, so a wide
        // character before the cursor counts two display columns and one index.
        let mut input = PromptInput::new("漢z");
        input.move_to_start();
        input.move_right();
        assert_eq!(input.before_cursor(), "漢");
        assert_eq!(input.char_at_cursor(), Some('z'));
        assert_eq!(input.after_cursor(), "");
    }

    #[test]
    fn prompt_kill_and_word_deletions_respect_the_cursor() {
        let mut input = PromptInput::new("cargo test --all-features");
        input.move_to_start();
        for _ in 0.."cargo ".chars().count() {
            input.move_right();
        }

        assert!(input.delete_to_start());
        assert_eq!(input.as_str(), "test --all-features");
        assert_eq!(input.cursor(), 0);

        input.move_to_end();
        assert!(input.delete_word_before_cursor());
        assert_eq!(input.as_str(), "test ");

        input.move_to_start();
        assert!(input.delete_to_end());
        assert_eq!(input.as_str(), "");
        assert!(!input.delete_to_end());
        assert!(!input.delete_before_cursor());
        assert!(!input.delete_at_cursor());
    }

    #[test]
    fn filtering_keys_off_the_whole_input_not_the_text_before_the_cursor() {
        let mut app = App::two_workspaces();
        app.begin_command_palette();
        for ch in "terminal".chars() {
            app.push_prompt_char(ch);
        }
        let from_end = app.active_command_palette_entries();

        app.move_prompt_cursor_to_start();
        app.move_prompt_cursor_right();
        let from_middle = app.active_command_palette_entries();

        assert_eq!(from_end, from_middle);
        assert!(!from_end.is_empty());
    }

    /// Ctrl+k is "select previous" where a prompt draws a list, and
    /// "delete to end of line" everywhere else; the two never both apply.
    /// F13: one wrap-around body, shared by every result list.
    #[test]
    fn a_list_selection_wraps_at_both_ends_and_empties_to_zero() {
        let mut selection = ListSelection::default();

        selection.step(1, 3);
        selection.step(1, 3);
        assert_eq!(selection.index(), 2);
        selection.step(1, 3);
        assert_eq!(
            selection.index(),
            0,
            "forward past the end wraps to the top"
        );
        selection.step(-1, 3);
        assert_eq!(selection.index(), 2, "back past the top wraps to the end");

        // A list that has just emptied has nothing to point at.
        selection.step(-1, 0);
        assert_eq!(selection.index(), 0);

        // A shorter list pulls the selection back inside it, which is what a
        // keystroke does to the palette's filtered entries.
        let mut selection = ListSelection::default();
        selection.step(4, 9);
        selection.clamp(2);
        assert_eq!(selection.index(), 1);
        selection.clamp(0);
        assert_eq!(selection.index(), 0);
    }

    /// F21: the step is modular for any `delta`, not only ±1. The backwards
    /// branch used to be `checked_sub(delta).unwrap_or(len - delta)`, which
    /// returns `2` for the first case below where a wrap returns `3`.
    #[test]
    fn a_list_selection_wraps_by_more_than_one_in_both_directions() {
        let step = |index: usize, delta: isize, len: usize| {
            let mut selection = ListSelection::default();
            selection.step(index as isize, len);
            selection.step(delta, len);
            selection.index()
        };

        // Backwards past the top by more than one.
        assert_eq!(step(1, -3, 5), 3);
        assert_eq!(step(0, -2, 5), 3);
        assert_eq!(step(4, -2, 5), 2);

        // Forwards past the end by more than one.
        assert_eq!(step(3, 3, 5), 1);
        assert_eq!(step(1, 4, 5), 0);

        // A step larger than the list is the same step modulo its length, in
        // both directions — including an exact multiple, which stays put.
        assert_eq!(step(2, 7, 5), step(2, 2, 5));
        assert_eq!(step(2, -7, 5), step(2, -2, 5));
        assert_eq!(step(2, 10, 5), 2);
        assert_eq!(step(2, -10, 5), 2);

        // A one-entry list has one answer whatever it is asked.
        assert_eq!(step(0, 9, 1), 0);
        assert_eq!(step(0, -9, 1), 0);

        // The extremes cannot overflow the arithmetic in either direction.
        assert_eq!(
            step(0, isize::MIN, 5),
            (5 - (isize::MIN.unsigned_abs() % 5)) % 5
        );
        assert_eq!(step(0, isize::MAX, 5), isize::MAX as usize % 5);
    }
}
