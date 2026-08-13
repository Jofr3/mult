//! The keybinding table, and the command palette generated from it.
//!
//! One table feeds both the palette and the help overlay, so a binding cannot
//! be added to one and forgotten in the other (E4).

use super::*;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAction {
    ShowKeybindings,
    FocusSidebar,
    FocusSelectedPane,
    StartInput,
    AddAgentChat,
    AddClaudeCodeChat,
    AddShellTerminal,
    AddCommandTerminal,
    OpenFileManager,
    OpenEditor,
    OpenWorkspace,
    DeleteSelected,
    SearchSelectedPane,
    ClearSearch,
    DismissNotices,
    ReloadConfig,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandPaletteEntry {
    pub action: CommandAction,
    pub label: &'static str,
    pub help: &'static str,
}

/// Which part of the overlay a binding is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingScope {
    /// Available when no prompt is open.
    Global,
    /// Available while a prompt or the command palette is open.
    Prompt,
    /// Not a key at all.
    Mouse,
}

impl BindingScope {
    pub fn title(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Prompt => "Prompts and the command palette",
            Self::Mouse => "Mouse",
        }
    }
}

/// What must hold for a binding's action to be offered by the command palette.
/// Bindings the palette cannot run (`action: None`) ignore this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingAvailability {
    Always,
    SelectedPane,
    SelectedWorkspace,
    DeletableSelection,
    ActiveSearch,
    PendingNotices,
}

/// One row of the single binding table.
///
/// The command palette and the help overlay are both generated from
/// [`BINDINGS`] so they cannot drift (E4): a row with an `action` is offered by
/// the palette, a row with `keys` is listed by the overlay, and most rows have
/// both. Rows with only `keys` are the bindings the palette has no action for
/// (navigation, scrolling, prompt editing); rows with only an `action` are
/// palette-only commands with no dedicated key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingEntry {
    /// The key or keys, or `None` for a palette-only command.
    pub keys: Option<&'static str>,
    pub label: &'static str,
    pub help: &'static str,
    /// The palette action, or `None` for a binding the palette cannot run.
    pub action: Option<CommandAction>,
    pub scope: BindingScope,
    availability: BindingAvailability,
}

impl BindingEntry {
    const fn global(
        keys: Option<&'static str>,
        label: &'static str,
        help: &'static str,
        action: Option<CommandAction>,
        availability: BindingAvailability,
    ) -> Self {
        Self {
            keys,
            label,
            help,
            action,
            scope: BindingScope::Global,
            availability,
        }
    }

    const fn reference(keys: &'static str, label: &'static str, scope: BindingScope) -> Self {
        Self {
            keys: Some(keys),
            label,
            help: "",
            action: None,
            scope,
            availability: BindingAvailability::Always,
        }
    }
}

/// Every binding and every command, in one place.
///
/// The order of the rows carrying an `action` is the order the command palette
/// shows them in, so this is also the palette's ordering.
pub const BINDINGS: &[BindingEntry] = &[
    BindingEntry::global(
        // `?` is conditional (see `App::help_key_opens_help`), `F1` is not.
        Some("? or F1"),
        "Show keybindings",
        "list every key and command in an overlay",
        Some(CommandAction::ShowKeybindings),
        BindingAvailability::Always,
    ),
    BindingEntry::global(
        None,
        "Focus sidebar",
        "return keyboard focus to workspace navigation",
        Some(CommandAction::FocusSidebar),
        BindingAvailability::Always,
    ),
    BindingEntry::global(
        None,
        "Focus selected pane",
        "move keyboard focus from sidebar to the selected chat or terminal",
        Some(CommandAction::FocusSelectedPane),
        BindingAvailability::SelectedPane,
    ),
    BindingEntry::global(
        None,
        "Start selected PTY",
        "start the selected chat/terminal PTY for immediate input",
        Some(CommandAction::StartInput),
        BindingAvailability::SelectedPane,
    ),
    BindingEntry::global(
        Some("Ctrl+s"),
        "Search selected pane",
        "filter terminal output or chat transcript lines",
        Some(CommandAction::SearchSelectedPane),
        BindingAvailability::SelectedPane,
    ),
    BindingEntry::global(
        Some("Ctrl+a"),
        "New pi agent chat",
        "add a pi agent chat to the selected workspace",
        Some(CommandAction::AddAgentChat),
        BindingAvailability::SelectedWorkspace,
    ),
    BindingEntry::global(
        Some("Ctrl+x"),
        "New Claude Code chat",
        "add a Claude Code agent chat to the selected workspace",
        Some(CommandAction::AddClaudeCodeChat),
        BindingAvailability::SelectedWorkspace,
    ),
    BindingEntry::global(
        Some("Ctrl+t"),
        "New shell terminal",
        "add a shell terminal to the selected workspace",
        Some(CommandAction::AddShellTerminal),
        BindingAvailability::SelectedWorkspace,
    ),
    BindingEntry::global(
        None,
        "New command terminal",
        "add a command/dev-server terminal to the selected workspace",
        Some(CommandAction::AddCommandTerminal),
        BindingAvailability::SelectedWorkspace,
    ),
    BindingEntry::global(
        Some("Ctrl+n"),
        "Open file manager",
        "open the configured file manager in the selected workspace's root directory",
        Some(CommandAction::OpenFileManager),
        BindingAvailability::SelectedWorkspace,
    ),
    BindingEntry::global(
        Some("Ctrl+e"),
        "Open editor",
        "open your editor ($VISUAL, $EDITOR, or editor_command) in the selected workspace's root directory",
        Some(CommandAction::OpenEditor),
        BindingAvailability::SelectedWorkspace,
    ),
    BindingEntry::global(
        Some("Ctrl+f"),
        "Open workspace",
        "import a workspace directory",
        Some(CommandAction::OpenWorkspace),
        BindingAvailability::Always,
    ),
    BindingEntry::global(
        Some("Ctrl+q"),
        "Delete selected item",
        "delete the selected chat/terminal or an empty workspace, immediately",
        Some(CommandAction::DeleteSelected),
        BindingAvailability::DeletableSelection,
    ),
    BindingEntry::global(
        None,
        "Clear search",
        "clear the active search/filter",
        Some(CommandAction::ClearSearch),
        BindingAvailability::ActiveSearch,
    ),
    // Palette-only since `Ctrl+n` went to the file manager: notices expire on
    // their own, so losing the shortcut costs a wait, not the ability to clear.
    BindingEntry::global(
        None,
        "Dismiss notices",
        "clear the status notices without waiting for them to expire",
        Some(CommandAction::DismissNotices),
        BindingAvailability::PendingNotices,
    ),
    BindingEntry::global(
        None,
        "Reload config",
        "re-read config.json and apply it without restarting",
        Some(CommandAction::ReloadConfig),
        BindingAvailability::Always,
    ),
    BindingEntry::global(
        Some("Ctrl+Esc"),
        "Quit mult",
        "save state and exit",
        Some(CommandAction::Quit),
        BindingAvailability::Always,
    ),
    // Bindings the palette has no action for. They are listed here rather than
    // in a second hardcoded list so the overlay covers everything (E4).
    BindingEntry::reference(
        "Ctrl+j or Ctrl+Enter",
        "Select next sidebar item",
        BindingScope::Global,
    ),
    BindingEntry::reference(
        "Ctrl+k",
        "Select previous sidebar item",
        BindingScope::Global,
    ),
    BindingEntry::reference("Ctrl+p", "Open the command palette", BindingScope::Global),
    BindingEntry::reference(
        "any other key",
        "start the selected chat/terminal PTY and send the key to it",
        BindingScope::Global,
    ),
    BindingEntry::reference("Enter", "Submit", BindingScope::Prompt),
    BindingEntry::reference("Esc or Ctrl+c", "Cancel", BindingScope::Prompt),
    BindingEntry::reference(
        "Left/Right",
        "Move the cursor one character",
        BindingScope::Prompt,
    ),
    BindingEntry::reference(
        "Home/End or Ctrl+a/Ctrl+e",
        "Move the cursor to the start/end",
        BindingScope::Prompt,
    ),
    BindingEntry::reference(
        "Backspace/Delete",
        "Delete the character before/after the cursor",
        BindingScope::Prompt,
    ),
    BindingEntry::reference(
        "Ctrl+w",
        "Delete the word before the cursor",
        BindingScope::Prompt,
    ),
    BindingEntry::reference(
        "Ctrl+u",
        "Delete everything before the cursor",
        BindingScope::Prompt,
    ),
    BindingEntry::reference(
        "Up/Down or Ctrl+k/Ctrl+j",
        "Move through results (palette, project list)",
        BindingScope::Prompt,
    ),
    BindingEntry::reference(
        "wheel",
        "Scroll the selected output pane",
        BindingScope::Mouse,
    ),
    BindingEntry::reference(
        "drag",
        "Select visible text and copy it with OSC 52",
        BindingScope::Mouse,
    ),
    BindingEntry::reference(
        "Ctrl+Shift+C",
        "Copy the active mult selection",
        BindingScope::Mouse,
    ),
];

impl App {
    pub fn command_palette_entries_for(&self, query: &str) -> Vec<CommandPaletteEntry> {
        let entries = self.available_command_palette_entries();
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return entries;
        }

        let terms = query.split_whitespace().collect::<Vec<_>>();
        entries
            .into_iter()
            .filter(|entry| {
                let haystack = format!("{} {}", entry.label, entry.help).to_ascii_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect()
    }

    pub fn active_command_palette_entries(&self) -> Vec<CommandPaletteEntry> {
        match self.prompt() {
            Some(Prompt::CommandPalette(prompt)) => {
                self.command_palette_entries_for(prompt.input.as_str())
            }
            _ => Vec::new(),
        }
    }

    fn available_command_palette_entries(&self) -> Vec<CommandPaletteEntry> {
        BINDINGS
            .iter()
            .filter(|binding| self.binding_is_available(binding.availability))
            .filter_map(|binding| {
                Some(CommandPaletteEntry {
                    action: binding.action?,
                    label: binding.label,
                    help: binding.help,
                })
            })
            .collect()
    }

    fn binding_is_available(&self, availability: BindingAvailability) -> bool {
        match availability {
            BindingAvailability::Always => true,
            BindingAvailability::SelectedPane => self.selected_main_focus().is_some(),
            BindingAvailability::SelectedWorkspace => self.selected_workspace_id().is_some(),
            BindingAvailability::DeletableSelection => self.selected_item_can_be_deleted(),
            BindingAvailability::ActiveSearch => self.active_search.is_some(),
            BindingAvailability::PendingNotices => !self.notices.is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_palette_filters_and_returns_existing_actions() {
        let mut app = App::default();

        app.begin_command_palette();
        for ch in "dev-server".chars() {
            app.push_prompt_char(ch);
        }

        let entries = app.active_command_palette_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].action, CommandAction::AddCommandTerminal);
        assert_eq!(
            app.submit_command_palette(),
            Some(CommandAction::AddCommandTerminal)
        );
        assert_eq!(app.prompt(), None);
    }

    // ---- E4: one binding table --------------------------------------------

    #[test]
    fn the_command_palette_is_generated_from_the_binding_table() {
        let app = App::default();
        let offered = app
            .command_palette_entries_for("")
            .into_iter()
            .map(|entry| (entry.action, entry.label, entry.help))
            .collect::<Vec<_>>();
        let from_table = BINDINGS
            .iter()
            .filter(|binding| binding.action.is_some())
            .filter(|binding| app.binding_is_available(binding.availability))
            .map(|binding| (binding.action.unwrap(), binding.label, binding.help))
            .collect::<Vec<_>>();

        assert_eq!(offered, from_table);
        assert!(!offered.is_empty());
    }

    #[test]
    fn every_binding_is_reachable_from_the_table_and_nothing_is_half_declared() {
        for binding in BINDINGS {
            assert!(
                binding.keys.is_some() || binding.action.is_some(),
                "{} declares neither a key nor an action",
                binding.label
            );
            assert!(!binding.label.is_empty());
        }

        // The overlay's job is the keys, so every key `mult` binds outside a
        // PTY must appear; these are the ones the README documented and the
        // palette does not cover.
        let keys = BINDINGS
            .iter()
            .filter_map(|binding| binding.keys)
            .collect::<Vec<_>>();
        for expected in [
            "Ctrl+p",
            "Ctrl+j or Ctrl+Enter",
            "Ctrl+k",
            "Ctrl+Esc",
            "Ctrl+s",
            "Ctrl+a",
            "Ctrl+x",
            "Ctrl+t",
            "Ctrl+n",
            "Ctrl+e",
            "Ctrl+f",
            "Ctrl+q",
            "? or F1",
        ] {
            assert!(keys.contains(&expected), "{expected} is not in the table");
        }
    }
}
