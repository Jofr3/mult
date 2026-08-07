//! Prompts: the text input every prompt shares, the editing operations applied
//! to it, and the lifecycles of the command palette and the command-terminal
//! prompt.

use super::*;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    OpenWorkspace(OpenWorkspacePrompt),
    NewTerminalCommand(TerminalCommandPrompt),
    CommandPalette(CommandPalettePrompt),
    Search(SearchPrompt),
}

/// A prompt's text together with its cursor.
///
/// The cursor is a **character** offset, never a byte offset: prompts hold
/// paths and shell commands, so multi-byte characters are ordinary input, and
/// slicing those by byte either panics or splits a character in half. Byte
/// offsets are derived from the character offset on demand (E7).
///
/// Grapheme clusters are deliberately out of scope here — a combining mark is
/// its own `char`, so the cursor can sit between a base character and its mark.
/// Cluster-aware motion is parked in `docs/ROADMAP.md`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptInput {
    text: String,
    /// Invariant: `0 ..= text.chars().count()`.
    cursor: usize,
}

/// One editing operation on a [`PromptInput`]. The four prompt key handlers all
/// translate their keys into these and hand them to [`App::apply_prompt_edit`],
/// so the cursor logic exists once (F13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptEdit {
    Insert(char),
    Backspace,
    DeleteForward,
    MoveLeft,
    MoveRight,
    MoveHome,
    MoveEnd,
    /// `Ctrl+w`: delete the whitespace-delimited word before the cursor.
    DeleteWordBefore,
    /// `Ctrl+u`: delete everything before the cursor.
    DeleteToStart,
}

impl PromptEdit {
    /// Whether the edit changes the text (as opposed to only moving the
    /// cursor). Only a change clears a prompt's error and resets its list
    /// selection; moving the cursor must not throw the user's selection away.
    fn mutates_text(self) -> bool {
        !matches!(
            self,
            Self::MoveLeft | Self::MoveRight | Self::MoveHome | Self::MoveEnd
        )
    }
}

impl PromptInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        Self { text, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The cursor's position in characters.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// Byte offset of character `index`, or the end of the string when `index`
    /// is past the last character.
    fn byte_offset(&self, index: usize) -> usize {
        self.text
            .char_indices()
            .nth(index)
            .map_or(self.text.len(), |(offset, _)| offset)
    }

    /// `(before, at, after)`: the text before the cursor, the single character
    /// the cursor sits on (empty past the end), and the text after it.
    /// Concatenating the three reproduces [`Self::as_str`] exactly, which is
    /// what lets the renderer style the cursor cell without rewriting the text.
    pub fn split_at_cursor(&self) -> (&str, &str, &str) {
        let start = self.byte_offset(self.cursor);
        let end = self.byte_offset(self.cursor.saturating_add(1));
        (
            &self.text[..start],
            &self.text[start..end],
            &self.text[end..],
        )
    }

    /// `pub(crate)` for the renderer's cursor tests; the app-level entry
    /// point is [`App::apply_prompt_edit`].
    pub(crate) fn apply(&mut self, edit: PromptEdit) -> bool {
        match edit {
            PromptEdit::Insert(ch) => {
                let at = self.byte_offset(self.cursor);
                self.text.insert(at, ch);
                self.cursor += 1;
                true
            }
            PromptEdit::Backspace => {
                if self.cursor == 0 {
                    return false;
                }
                let end = self.byte_offset(self.cursor);
                let start = self.byte_offset(self.cursor - 1);
                self.text.replace_range(start..end, "");
                self.cursor -= 1;
                true
            }
            PromptEdit::DeleteForward => {
                let start = self.byte_offset(self.cursor);
                let end = self.byte_offset(self.cursor.saturating_add(1));
                if start == end {
                    return false;
                }
                self.text.replace_range(start..end, "");
                true
            }
            PromptEdit::MoveLeft => {
                if self.cursor == 0 {
                    return false;
                }
                self.cursor -= 1;
                true
            }
            PromptEdit::MoveRight => {
                if self.cursor >= self.char_count() {
                    return false;
                }
                self.cursor += 1;
                true
            }
            PromptEdit::MoveHome => {
                let moved = self.cursor != 0;
                self.cursor = 0;
                moved
            }
            PromptEdit::MoveEnd => {
                let end = self.char_count();
                let moved = self.cursor != end;
                self.cursor = end;
                moved
            }
            PromptEdit::DeleteWordBefore => {
                if self.cursor == 0 {
                    return false;
                }
                let chars = self.text.chars().collect::<Vec<_>>();
                let mut index = self.cursor;
                while index > 0 && chars[index - 1].is_whitespace() {
                    index -= 1;
                }
                while index > 0 && !chars[index - 1].is_whitespace() {
                    index -= 1;
                }
                let start = self.byte_offset(index);
                let end = self.byte_offset(self.cursor);
                self.text.replace_range(start..end, "");
                self.cursor = index;
                true
            }
            PromptEdit::DeleteToStart => {
                if self.cursor == 0 {
                    return false;
                }
                let end = self.byte_offset(self.cursor);
                self.text.replace_range(..end, "");
                self.cursor = 0;
                true
            }
        }
    }
}

/// A wrap-around selection index over a list whose length is supplied by the
/// caller (the entries are recomputed from the filter on every keystroke, so
/// the selection cannot cache them).
///
/// This replaces four copies of the same body in the prompt handlers (F13), and
/// its backwards step is a real modular wrap rather than
/// `index.checked_sub(delta).unwrap_or(len - delta)`, which only happened to be
/// right for `delta == 1` and underflowed for `delta > len` (F21).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListSelection {
    index: usize,
}

impl ListSelection {
    pub fn index(self) -> usize {
        self.index
    }

    pub fn reset(&mut self) {
        self.index = 0;
    }

    /// Keep the selection inside `0..len`, or at 0 for an empty list.
    pub fn clamp(&mut self, len: usize) {
        self.index = self.index.min(len.saturating_sub(1));
    }

    /// Move `delta` entries, wrapping in both directions.
    pub fn step(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.index = 0;
            return;
        }
        let modulus = isize::try_from(len).unwrap_or(isize::MAX);
        let current = isize::try_from(self.index % len).unwrap_or(0);
        let offset = delta.rem_euclid(modulus);
        let next = current.saturating_add(offset).rem_euclid(modulus);
        self.index = usize::try_from(next).unwrap_or(0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommandPrompt {
    pub input: PromptInput,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPalettePrompt {
    pub input: PromptInput,
    pub selected: ListSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPrompt {
    pub input: PromptInput,
    pub scope: SearchScope,
    pub error: Option<String>,
}

impl App {
    pub fn begin_command_palette(&mut self) {
        self.set_prompt(Prompt::CommandPalette(CommandPalettePrompt {
            input: PromptInput::default(),
            selected: ListSelection::default(),
        }));
    }

    pub fn select_next_command_palette_entry(&mut self) {
        self.move_command_palette_selection(1);
    }

    pub fn select_previous_command_palette_entry(&mut self) {
        self.move_command_palette_selection(-1);
    }

    pub fn submit_command_palette(&mut self) -> Option<CommandAction> {
        let Some(Prompt::CommandPalette(prompt)) = self.prompt() else {
            return None;
        };
        let entries = self.command_palette_entries_for(prompt.input.as_str());
        let action = entries
            .get(prompt.selected.index().min(entries.len().saturating_sub(1)))
            .map(|entry| entry.action);
        self.clear_prompt();
        action
    }

    fn move_command_palette_selection(&mut self, delta: isize) {
        let Some(Prompt::CommandPalette(prompt)) = self.prompt() else {
            return;
        };
        let len = self
            .command_palette_entries_for(prompt.input.as_str())
            .len();
        if let Some(Prompt::CommandPalette(prompt)) = self.prompt_mut() {
            prompt.selected.step(delta, len);
        }
    }

    fn clamp_command_palette_selection(&mut self) {
        let Some(Prompt::CommandPalette(prompt)) = self.prompt() else {
            return;
        };
        let len = self
            .command_palette_entries_for(prompt.input.as_str())
            .len();
        if let Some(Prompt::CommandPalette(prompt)) = self.prompt_mut() {
            prompt.selected.clamp(len);
        }
    }

    pub fn begin_new_terminal_command(&mut self) -> bool {
        if self.selected_workspace_id().is_none() {
            return false;
        }

        self.set_prompt(Prompt::NewTerminalCommand(TerminalCommandPrompt {
            input: PromptInput::default(),
            error: None,
        }));
        true
    }

    pub fn cancel_prompt(&mut self) {
        self.clear_prompt();
    }

    /// The one place prompt text is edited (E7/F13).
    ///
    /// Every prompt variant that carries text shares this body, so the cursor,
    /// the motions and the kill commands exist once rather than four times.
    /// Only edits that *change* the text clear the prompt's error and reset its
    /// list selection: moving the cursor must not throw away the entry the user
    /// had picked, and the fuzzy filter keys off the whole input, not the part
    /// before the cursor.
    pub fn apply_prompt_edit(&mut self, edit: PromptEdit) -> bool {
        let mutated = match self.prompt_mut() {
            Some(Prompt::OpenWorkspace(prompt)) => {
                let changed = prompt.input.apply(edit);
                if changed && edit.mutates_text() {
                    prompt.error = None;
                    prompt.selected.reset();
                }
                changed
            }
            Some(Prompt::NewTerminalCommand(prompt)) => {
                let changed = prompt.input.apply(edit);
                if changed && edit.mutates_text() {
                    prompt.error = None;
                }
                changed
            }
            Some(Prompt::CommandPalette(prompt)) => {
                let changed = prompt.input.apply(edit);
                if changed && edit.mutates_text() {
                    prompt.selected.reset();
                }
                changed
            }
            Some(Prompt::Search(prompt)) => {
                let changed = prompt.input.apply(edit);
                if changed && edit.mutates_text() {
                    prompt.error = None;
                }
                changed
            }
            _ => false,
        };
        self.clamp_command_palette_selection();
        mutated
    }

    pub fn push_prompt_char(&mut self, c: char) {
        self.apply_prompt_edit(PromptEdit::Insert(c));
    }

    #[cfg(test)]
    pub(crate) fn pop_prompt_char(&mut self) {
        self.apply_prompt_edit(PromptEdit::Backspace);
    }

    /// The active prompt's text and cursor, for the renderer.
    pub fn prompt_input(&self) -> Option<&PromptInput> {
        match self.prompt() {
            Some(Prompt::OpenWorkspace(prompt)) => Some(&prompt.input),
            Some(Prompt::NewTerminalCommand(prompt)) => Some(&prompt.input),
            Some(Prompt::CommandPalette(prompt)) => Some(&prompt.input),
            Some(Prompt::Search(prompt)) => Some(&prompt.input),
            None => None,
        }
    }

    pub fn submit_new_terminal_command(&mut self) {
        let Some(Prompt::NewTerminalCommand(prompt)) = self.prompt() else {
            return;
        };
        let command = prompt.input.as_str().trim().to_string();
        if command.is_empty() {
            self.set_terminal_command_error("enter a command to run");
            return;
        }

        let Some(workspace) = self.selected_workspace_id() else {
            self.set_terminal_command_error("select a workspace first");
            return;
        };

        let next = self
            .project
            .workspace(workspace)
            .map(|workspace| workspace.terminals.len() + 1)
            .unwrap_or(1);
        let name = command_terminal_name(&command, next);

        match self
            .project
            .add_command_terminal(workspace, name.clone(), command)
        {
            Ok(Some(terminal)) => {
                self.clear_prompt();
                self.select_item(NavItem::Terminal {
                    workspace,
                    terminal,
                });
                self.clear_operation_error();
                self.mark_structural_change();
            }
            Ok(None) => self.set_terminal_command_error("selected workspace no longer exists"),
            Err(error) => self.set_terminal_command_error(error.to_string()),
        }
    }

    fn set_terminal_command_error(&mut self, message: impl Into<String>) {
        if let Some(Prompt::NewTerminalCommand(prompt)) = self.prompt_mut() {
            prompt.error = Some(message.into());
        }
    }
}

fn command_terminal_name(command: &str, next: usize) -> String {
    let command = command.trim();
    if command.is_empty() {
        format!("command-{next}")
    } else {
        command.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_terminal_prompt_adds_command_terminal() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;

        assert!(app.begin_new_terminal_command());
        app.push_prompt_char('c');
        app.push_prompt_char('a');
        app.push_prompt_char('r');
        app.push_prompt_char('g');
        app.push_prompt_char('o');
        app.push_prompt_char(' ');
        app.push_prompt_char('t');
        app.push_prompt_char('e');
        app.push_prompt_char('s');
        app.push_prompt_char('t');
        app.submit_new_terminal_command();

        let terminal = app.project.workspaces[0].terminals.last().unwrap();
        assert_eq!(terminal.name, "cargo test");
        assert_eq!(
            terminal.launch,
            crate::model::TerminalLaunch::Command("cargo test".to_string())
        );
        assert_eq!(
            app.selected_item(),
            Some(NavItem::Terminal {
                workspace,
                terminal: terminal.id,
            })
        );
        assert_eq!(app.focus(), Some(FocusMode::Terminal));
        assert!(app.is_dirty());
    }

    #[test]
    fn prompt_input_can_be_edited() {
        let mut app = App::default();
        app.begin_open_workspace(&[]);
        if let Some(Prompt::OpenWorkspace(prompt)) = app.prompt_mut_for_test() {
            prompt.input = PromptInput::default();
        }

        app.push_prompt_char('/');
        app.push_prompt_char('t');
        app.pop_prompt_char();

        assert_eq!(
            app.prompt().cloned(),
            Some(Prompt::OpenWorkspace(OpenWorkspacePrompt {
                input: PromptInput::new("/"),
                error: None,
                selected: ListSelection::default(),
                mode: OpenWorkspaceMode::Path,
            }))
        );
    }

    // ---- E7: the prompt cursor -------------------------------------------

    /// The prompt cursor indexes characters, not bytes. A byte index into
    /// "café/日本" either panics or splits a character, and the Open-Workspace
    /// prompt pre-fills the working directory, so long paths with non-ASCII in
    /// them are the ordinary case rather than an exotic one.
    #[test]
    fn prompt_editing_is_on_character_boundaries_for_multibyte_and_wide_text() {
        let mut input = PromptInput::new("café/日本");
        assert_eq!(
            input.cursor(),
            7,
            "the cursor starts past the last character"
        );

        // Left twice lands between the two wide characters, not inside one.
        assert!(input.apply(PromptEdit::MoveLeft));
        assert!(input.apply(PromptEdit::MoveLeft));
        let (before, at, after) = input.split_at_cursor();
        assert_eq!((before, at, after), ("café/", "日", "本"));
        assert_eq!(format!("{before}{at}{after}"), "café/日本");

        // Backspace removes the whole `/`, and the accented character before it
        // survives intact.
        assert!(input.apply(PromptEdit::Backspace));
        assert_eq!(input.as_str(), "café日本");
        assert!(input.apply(PromptEdit::Backspace));
        assert_eq!(input.as_str(), "caf日本");
        assert_eq!(input.cursor(), 3);

        // Inserting at the cursor puts the character between the two halves,
        // not at a byte offset that happens to be there.
        assert!(input.apply(PromptEdit::Insert('é')));
        assert_eq!(input.as_str(), "café日本");

        // Forward delete removes one whole wide character.
        assert!(input.apply(PromptEdit::DeleteForward));
        assert_eq!(input.as_str(), "café本");

        // Home/End are the ends of the *string*, and the cursor past the end
        // reports an empty character under it.
        assert!(input.apply(PromptEdit::MoveHome));
        assert_eq!(input.split_at_cursor(), ("", "c", "afé本"));
        assert!(input.apply(PromptEdit::MoveEnd));
        assert_eq!(input.split_at_cursor(), ("café本", "", ""));
        assert!(!input.apply(PromptEdit::MoveRight));
        assert!(!input.apply(PromptEdit::DeleteForward));
    }

    #[test]
    fn prompt_word_and_line_kills_stop_at_the_cursor() {
        let mut input = PromptInput::new("/home/user/my project/src");
        for _ in 0..4 {
            assert!(input.apply(PromptEdit::MoveLeft));
        }
        assert_eq!(input.split_at_cursor().0, "/home/user/my project");

        // Ctrl+w takes the whitespace-delimited word before the cursor and
        // nothing after it.
        assert!(input.apply(PromptEdit::DeleteWordBefore));
        assert_eq!(input.as_str(), "/home/user/my /src");
        assert_eq!(input.cursor(), 14);

        // Ctrl+u takes everything before the cursor, again leaving the tail.
        assert!(input.apply(PromptEdit::DeleteToStart));
        assert_eq!(input.as_str(), "/src");
        assert_eq!(input.cursor(), 0);
        assert!(!input.apply(PromptEdit::DeleteToStart));
        assert!(!input.apply(PromptEdit::Backspace));
    }

    #[test]
    fn prompt_filtering_keys_off_the_whole_input_not_the_text_before_the_cursor() {
        let mut app = App::default();
        app.begin_command_palette();
        for ch in "quit".chars() {
            app.push_prompt_char(ch);
        }
        assert_eq!(
            app.active_command_palette_entries()
                .iter()
                .map(|entry| entry.action)
                .collect::<Vec<_>>(),
            vec![CommandAction::Quit]
        );

        // Moving the cursor into the middle of the query must not narrow the
        // filter to "qu" — the user is about to fix a typo, not re-search.
        app.apply_prompt_edit(PromptEdit::MoveLeft);
        app.apply_prompt_edit(PromptEdit::MoveLeft);
        assert_eq!(
            app.active_command_palette_entries()
                .iter()
                .map(|entry| entry.action)
                .collect::<Vec<_>>(),
            vec![CommandAction::Quit]
        );
    }

    #[test]
    fn a_cursor_move_keeps_the_selected_entry_but_an_edit_resets_it() {
        let mut app = App::default();
        app.begin_command_palette();
        app.select_next_command_palette_entry();
        app.select_next_command_palette_entry();
        let Some(Prompt::CommandPalette(prompt)) = app.prompt() else {
            panic!("palette is open");
        };
        assert_eq!(prompt.selected.index(), 2);

        app.apply_prompt_edit(PromptEdit::MoveHome);
        let Some(Prompt::CommandPalette(prompt)) = app.prompt() else {
            panic!("palette is open");
        };
        assert_eq!(prompt.selected.index(), 2, "a motion is not a new query");

        app.push_prompt_char('f');
        let Some(Prompt::CommandPalette(prompt)) = app.prompt() else {
            panic!("palette is open");
        };
        assert_eq!(
            prompt.selected.index(),
            0,
            "a new query selects the best match"
        );
    }

    // ---- F21: modular wrap ------------------------------------------------

    #[test]
    fn list_selection_wraps_modularly_in_both_directions() {
        let mut selection = ListSelection::default();

        // Forwards past the end wraps.
        selection.step(3, 5);
        assert_eq!(selection.index(), 3);
        selection.step(4, 5);
        assert_eq!(selection.index(), 2);

        // Backwards by more than one — the case the old
        // `checked_sub(delta).unwrap_or(len - delta)` body got wrong. From 2,
        // -4 over 5 entries is 3, not `len - delta`.
        selection.step(-4, 5);
        assert_eq!(selection.index(), 3);
        selection.step(-3, 5);
        assert_eq!(selection.index(), 0);
        selection.step(-1, 5);
        assert_eq!(selection.index(), 4);

        // A delta larger than the list is reduced, in both directions, instead
        // of underflowing.
        selection.step(12, 5);
        assert_eq!(selection.index(), 1);
        selection.step(-12, 5);
        assert_eq!(selection.index(), 4);
        // A stale index from a longer list is reduced first, then stepped.
        selection.step(-7, 3);
        assert_eq!(selection.index(), 0);

        // Extremes must not panic.
        selection.step(isize::MIN, 5);
        selection.step(isize::MAX, 5);
        assert!(selection.index() < 5);

        // An empty list has no position to be in.
        selection.step(-3, 0);
        assert_eq!(selection.index(), 0);
        selection.step(3, 0);
        assert_eq!(selection.index(), 0);
    }

    #[test]
    fn list_selection_clamps_into_a_shrinking_list() {
        let mut selection = ListSelection::default();
        selection.step(4, 5);
        selection.clamp(2);
        assert_eq!(selection.index(), 1);
        selection.clamp(0);
        assert_eq!(selection.index(), 0);
    }
}
