//! The single keybinding table (E4).
//!
//! The command palette and the `?`/`F1` help overlay are both generated from
//! [`BINDINGS`], so a command that gains a key, or a key that changes, cannot
//! appear in one and not the other.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAction {
    FocusSidebar,
    FocusSelectedPane,
    StartInput,
    AddAgentChat,
    AddClaudeCodeChat,
    AddShellTerminal,
    AddCommandTerminal,
    OpenWorkspace,
    DeleteSelected,
    SearchSelectedPane,
    ClearSearch,
    ReloadConfig,
    DismissStatusNotice,
    ShowHelp,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandPaletteEntry {
    pub action: CommandAction,
    pub label: &'static str,
    pub help: &'static str,
}

/// Which group of the help overlay a [`Binding`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpSection {
    Navigation,
    Sessions,
    View,
    Prompts,
    Mouse,
}

impl HelpSection {
    pub const ALL: [Self; 5] = [
        Self::Navigation,
        Self::Sessions,
        Self::View,
        Self::Prompts,
        Self::Mouse,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Navigation => "Navigation",
            Self::Sessions => "Chats, terminals and workspaces",
            Self::View => "View",
            Self::Prompts => "Prompts",
            Self::Mouse => "Mouse",
        }
    }
}

/// When the command palette may offer a [`Binding`]'s action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Always,
    /// A chat or terminal is selected.
    SelectedPane,
    /// There is a workspace to add to.
    Workspace,
    /// The selection (or an empty workspace) can be deleted.
    DeletableSelection,
    ActiveSearch,
    StatusNotice,
}

/// One row of the single keybinding table (E4).
///
/// The command palette and the `?`/`F1` help overlay are both generated from
/// [`BINDINGS`], so a command that gains a key, or a key that changes, cannot
/// appear in one and not the other. Rows with no `action` are keys the palette
/// cannot run — sidebar movement, scrolling, mouse selection, prompt editing —
/// and rows with an empty `keys` are palette-only commands. Both kinds belong
/// in the table rather than in a second hand-kept list in the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub section: HelpSection,
    pub keys: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub action: Option<CommandAction>,
    pub availability: Availability,
}

impl Binding {
    pub(super) fn palette_entry(&self) -> Option<CommandPaletteEntry> {
        Some(CommandPaletteEntry {
            action: self.action?,
            label: self.label,
            help: self.help,
        })
    }
}

/// Every binding and every command, in command-palette order. The help overlay
/// re-groups this by [`HelpSection`]; the palette filters it by
/// [`Availability`] and keeps this order.
pub const BINDINGS: &[Binding] = &[
    Binding {
        section: HelpSection::Navigation,
        keys: "",
        label: "Focus sidebar",
        help: "return keyboard focus to workspace navigation",
        action: Some(CommandAction::FocusSidebar),
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Navigation,
        keys: "",
        label: "Focus selected pane",
        help: "move keyboard focus from sidebar to the selected chat or terminal",
        action: Some(CommandAction::FocusSelectedPane),
        availability: Availability::SelectedPane,
    },
    Binding {
        section: HelpSection::Navigation,
        keys: "any key",
        label: "Start selected PTY",
        help: "start the selected chat/terminal PTY for immediate input",
        action: Some(CommandAction::StartInput),
        availability: Availability::SelectedPane,
    },
    Binding {
        section: HelpSection::View,
        keys: "Ctrl+s",
        label: "Search selected pane",
        help: "filter terminal output or chat transcript lines",
        action: Some(CommandAction::SearchSelectedPane),
        availability: Availability::SelectedPane,
    },
    Binding {
        section: HelpSection::Sessions,
        keys: "Ctrl+a",
        label: "New pi agent chat",
        help: "add a pi agent chat to the selected workspace",
        action: Some(CommandAction::AddAgentChat),
        availability: Availability::Workspace,
    },
    Binding {
        section: HelpSection::Sessions,
        keys: "Ctrl+x",
        label: "New Claude Code chat",
        help: "add a Claude Code agent chat to the selected workspace",
        action: Some(CommandAction::AddClaudeCodeChat),
        availability: Availability::Workspace,
    },
    Binding {
        section: HelpSection::Sessions,
        keys: "Ctrl+t",
        label: "New shell terminal",
        help: "add a shell terminal to the selected workspace",
        action: Some(CommandAction::AddShellTerminal),
        availability: Availability::Workspace,
    },
    Binding {
        section: HelpSection::Sessions,
        keys: "",
        label: "New command terminal",
        help: "add a command/dev-server terminal to the selected workspace",
        action: Some(CommandAction::AddCommandTerminal),
        availability: Availability::Workspace,
    },
    Binding {
        section: HelpSection::Sessions,
        keys: "Ctrl+f",
        label: "Open workspace",
        help: "import a workspace directory",
        action: Some(CommandAction::OpenWorkspace),
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Sessions,
        keys: "Ctrl+q",
        label: "Delete selected item",
        help: "delete the selected chat/terminal or an empty workspace (asks first)",
        action: Some(CommandAction::DeleteSelected),
        availability: Availability::DeletableSelection,
    },
    Binding {
        section: HelpSection::View,
        keys: "",
        label: "Clear search",
        help: "clear the active search/filter",
        action: Some(CommandAction::ClearSearch),
        availability: Availability::ActiveSearch,
    },
    Binding {
        section: HelpSection::View,
        keys: "Ctrl+g",
        label: "Dismiss status message",
        help: "clear the message in the status line (also Ctrl+g)",
        action: Some(CommandAction::DismissStatusNotice),
        availability: Availability::StatusNotice,
    },
    Binding {
        section: HelpSection::View,
        keys: "",
        label: "Reload config",
        help: "re-read config.json and apply it without restarting",
        action: Some(CommandAction::ReloadConfig),
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Navigation,
        keys: "? / F1",
        label: "Show keybindings",
        help: "open this list of keys and commands",
        action: Some(CommandAction::ShowHelp),
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Navigation,
        keys: "Ctrl+Esc",
        label: "Quit mult",
        help: "save state and exit",
        action: Some(CommandAction::Quit),
        availability: Availability::Always,
    },
    // Rows below have no palette action: they are raw key or mouse handling.
    Binding {
        section: HelpSection::Navigation,
        keys: "Ctrl+j / Ctrl+Enter",
        label: "Select next item",
        help: "move the sidebar selection down",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Navigation,
        keys: "Ctrl+k",
        label: "Select previous item",
        help: "move the sidebar selection up",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Navigation,
        keys: "Ctrl+p",
        label: "Command palette",
        help: "search and run every command in this list",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Prompts,
        keys: "Enter",
        label: "Submit",
        help: "run the prompt, or confirm a delete",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Prompts,
        keys: "Esc / Ctrl+c",
        label: "Cancel",
        help: "close the prompt and change nothing",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Prompts,
        keys: "← / →",
        label: "Move the cursor",
        help: "move one character left or right",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Prompts,
        keys: "Home/End, Ctrl+a/Ctrl+e",
        label: "Jump to start/end",
        help: "move the cursor to either end of the input",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Prompts,
        keys: "Backspace / Delete",
        label: "Delete a character",
        help: "delete before or under the cursor",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Prompts,
        keys: "Ctrl+w / Ctrl+u",
        label: "Delete a word / to start",
        help: "delete the word before the cursor, or everything before it",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Prompts,
        keys: "Ctrl+k",
        label: "Delete to end",
        help: "delete from the cursor to the end — in list prompts it selects the previous match instead",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Prompts,
        keys: "↑/↓, Ctrl+k/Ctrl+j",
        label: "Move through results",
        help: "move the selection in the palette and project prompts",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Mouse,
        keys: "wheel",
        label: "Scroll output",
        help: "scroll the chat or terminal output under the pointer",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Mouse,
        keys: "drag",
        label: "Select text",
        help: "drag over output to select it and copy it through OSC 52",
        action: None,
        availability: Availability::Always,
    },
    Binding {
        section: HelpSection::Mouse,
        keys: "Ctrl+Shift+C",
        label: "Copy selection",
        help: "copy the active mult selection when the terminal forwards the key",
        action: None,
        availability: Availability::Always,
    },
];

/// One rendered row of the help overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpRow {
    Heading(&'static str),
    Binding(&'static Binding),
}

/// The overlay's contents: every [`BINDINGS`] row, grouped by section. Nothing
/// here is written twice — the palette reads the same table.
pub fn keybinding_help_rows() -> Vec<HelpRow> {
    let mut rows = Vec::new();
    for section in HelpSection::ALL {
        let mut bindings = BINDINGS
            .iter()
            .filter(|binding| binding.section == section)
            .peekable();
        if bindings.peek().is_none() {
            continue;
        }
        rows.push(HelpRow::Heading(section.title()));
        rows.extend(bindings.map(HelpRow::Binding));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    #[test]
    fn the_palette_offers_reload_and_only_offers_dismiss_when_there_is_something_to_dismiss() {
        let mut app = App::two_workspaces();
        let actions = |app: &App| {
            app.command_palette_entries_for("")
                .into_iter()
                .map(|entry| entry.action)
                .collect::<Vec<_>>()
        };

        assert!(actions(&app).contains(&CommandAction::ReloadConfig));
        assert!(!actions(&app).contains(&CommandAction::DismissStatusNotice));

        app.set_last_error("something went wrong");

        assert!(actions(&app).contains(&CommandAction::DismissStatusNotice));
    }

    #[test]
    fn the_palette_and_the_help_overlay_are_generated_from_one_table() {
        let app = App::two_workspaces();
        let palette_actions = app
            .command_palette_entries_for("")
            .into_iter()
            .map(|entry| entry.action)
            .collect::<Vec<_>>();

        // Every command the palette offers is a row of the shared table, with
        // the same label and help text — there is no second list to drift.
        for action in &palette_actions {
            let binding = BINDINGS
                .iter()
                .find(|binding| binding.action == Some(*action))
                .unwrap_or_else(|| panic!("{action:?} is not in BINDINGS"));
            let entry = app
                .command_palette_entries_for("")
                .into_iter()
                .find(|entry| entry.action == *action)
                .expect("entry exists");
            assert_eq!(entry.label, binding.label);
            assert_eq!(entry.help, binding.help);
        }

        // And the overlay covers the whole table, including the rows the
        // palette cannot run (sidebar movement, prompt editing, the mouse).
        let rows = keybinding_help_rows();
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, HelpRow::Binding(_)))
                .count(),
            BINDINGS.len()
        );
        let listed = |label: &str| {
            rows.iter().any(|row| match row {
                HelpRow::Binding(binding) => binding.label == label,
                HelpRow::Heading(_) => false,
            })
        };
        assert!(listed("Command palette"));
        assert!(listed("Select next item"));
        assert!(listed("Scroll output"));
        assert!(listed("Quit mult"));
        assert!(palette_actions.contains(&CommandAction::ShowHelp));
    }
}
