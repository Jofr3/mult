//! Every modal prompt: its state, its editing, and what submitting it does.
//!
//! A prompt is one arm of [`super::InteractionMode`], so exactly one can be open
//! and no pane is focused while one is (F5). The editing keys are shared — the
//! cursor is a *character* index, never a byte offset, because the
//! open-workspace prompt is pre-filled with a path that may hold multi-byte
//! characters (E7).

use super::{
    command_terminal_name, keybinding_help_rows, App, Availability, Binding, CommandAction,
    CommandPaletteEntry, DeleteTarget, ListSelection, NavItem, OpenWorkspaceMode,
    OpenWorkspacePrompt, PromptInput, SearchScope, BINDINGS,
};
use crate::model::{TerminalId, WorkspaceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    OpenWorkspace(OpenWorkspacePrompt),
    NewTerminalCommand(TerminalCommandPrompt),
    CommandPalette(CommandPalettePrompt),
    Search(SearchPrompt),
    /// Confirmation for a destructive delete (E3).
    ConfirmDelete(ConfirmDeletePrompt),
    /// Confirmation before replaying command terminals out of `state.json`
    /// (C1). The state file is an execution boundary, so what it is about to run
    /// is shown and approved, never replayed unattended.
    ConfirmRestore(ConfirmRestorePrompt),
    /// The keybinding overlay (E4). Modal like every other prompt, so it cannot
    /// swallow input meant for a focused PTY while it is open.
    Help(HelpPrompt),
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

/// The "are you sure" step in front of a delete (E3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmDeletePrompt {
    pub target: DeleteTarget,
    /// Exactly what is about to go, named — e.g. `chat "agent" in workspace
    /// "orbit" (12 messages)`.
    pub summary: String,
    /// Set when the parent workspace is removed along with it, which is the
    /// part of the old behaviour nobody asked for and nothing announced.
    pub cascade: Option<String>,
}

/// One persisted `Command` terminal waiting to be approved (C1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRestore {
    pub workspace: WorkspaceId,
    pub terminal: TerminalId,
    pub name: String,
    /// The exact command line the login shell would be given. Shown verbatim:
    /// approving something you have not been shown is not a confirmation.
    pub command: String,
}

/// The startup confirmation in front of replaying persisted command terminals
/// (C1).
///
/// `state.json` is reachable by anything that can write the user's data
/// directory — a synced dotfile repository, a shared `$XDG_DATA_HOME`, any
/// same-uid process — and every terminal it marks `Running` used to be handed
/// straight to `$SHELL -lc` at startup, unshown and unasked. Shell terminals
/// still restore automatically (their command comes from `$SHELL`, not from the
/// file); anything with a stored command line waits here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmRestorePrompt {
    pub entries: Vec<PendingRestore>,
}

/// Scroll state for the keybinding overlay (E4). The overlay is longer than a
/// short terminal, so it scrolls rather than silently hiding its tail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HelpPrompt {
    pub scroll: usize,
}

impl App {
    /// Ask before replaying persisted command terminals (C1).
    ///
    /// The terminals are marked stopped up front, so declining — or quitting at
    /// the prompt, or a crash — leaves nothing pretending to run. Approval is
    /// what starts them.
    pub fn request_restore_confirmation(&mut self, entries: Vec<PendingRestore>) {
        if entries.is_empty() {
            return;
        }
        for entry in &entries {
            self.set_terminal_restore_on_launch(entry.terminal, false);
        }
        self.open_prompt(Prompt::ConfirmRestore(ConfirmRestorePrompt { entries }));
    }

    /// Approve the open restore prompt, returning what the caller should start.
    ///
    /// This is the user seeing each command verbatim and saying yes, so it is
    /// also what lifts the automatic-start block those terminals carry (C1).
    pub fn confirm_restore(&mut self) -> Vec<PendingRestore> {
        let Some(Prompt::ConfirmRestore(prompt)) = self.take_prompt() else {
            return Vec::new();
        };
        for entry in &prompt.entries {
            self.approve_command_terminal(entry.terminal);
        }
        prompt.entries
    }

    /// Decline the open restore prompt. Returns how many terminals were left
    /// stopped, so the caller can say so rather than leaving the user guessing
    /// why their terminals are idle.
    ///
    /// Declining is a refusal that has to *stick*: dropping the prompt was all
    /// this used to do, so the next loop tick auto-started the very command the
    /// user had just refused, and a key pressed at the pane did the same
    /// (C1/F1). The terminals stay in `unapproved_command_terminals`, which
    /// every automatic start path consults.
    pub fn decline_restore(&mut self) -> usize {
        let Some(Prompt::ConfirmRestore(prompt)) = self.take_prompt() else {
            return 0;
        };
        prompt.entries.len()
    }

    /// Whether `terminal` holds a command line out of `state.json` that no
    /// automatic path may run yet (C1). See `unapproved_command_terminals`.
    pub fn command_terminal_needs_approval(&self, terminal: TerminalId) -> bool {
        self.unapproved_command_terminals.contains(&terminal)
    }

    /// Record that the user has explicitly asked for `terminal` to run, so the
    /// automatic paths stop refusing it for the rest of the session.
    pub fn approve_command_terminal(&mut self, terminal: TerminalId) {
        self.unapproved_command_terminals.remove(&terminal);
    }

    pub fn begin_command_palette(&mut self) {
        self.open_prompt(Prompt::CommandPalette(CommandPalettePrompt {
            input: PromptInput::default(),
            selected: ListSelection::default(),
        }));
    }

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
        match self.mode.prompt() {
            Some(Prompt::CommandPalette(prompt)) => self.command_palette_entries_for(&prompt.input),
            _ => Vec::new(),
        }
    }

    pub fn select_next_command_palette_entry(&mut self) {
        self.move_command_palette_selection(1);
    }

    pub fn select_previous_command_palette_entry(&mut self) {
        self.move_command_palette_selection(-1);
    }

    pub fn submit_command_palette(&mut self) -> Option<CommandAction> {
        let Some(Prompt::CommandPalette(prompt)) = self.mode.prompt() else {
            return None;
        };
        let entries = self.command_palette_entries_for(&prompt.input);
        let action = entries
            .get(prompt.selected.index().min(entries.len().saturating_sub(1)))
            .map(|entry| entry.action);
        self.close_prompt();
        action
    }

    fn available_command_palette_entries(&self) -> Vec<CommandPaletteEntry> {
        BINDINGS
            .iter()
            .filter(|binding| self.binding_is_available(binding.availability))
            .filter_map(Binding::palette_entry)
            .collect()
    }

    /// Whether a [`Binding`]'s precondition holds right now. The help overlay
    /// lists every binding regardless; only the palette filters.
    fn binding_is_available(&self, availability: Availability) -> bool {
        match availability {
            Availability::Always => true,
            Availability::SelectedPane => self.selected_main_focus().is_some(),
            Availability::Workspace => self.selected_workspace_id().is_some(),
            Availability::DeletableSelection => self.selected_item_can_be_deleted(),
            Availability::ActiveSearch => self.active_search.is_some(),
            Availability::StatusNotice => !self.status_notices.is_empty(),
        }
    }

    fn move_command_palette_selection(&mut self, delta: isize) {
        let Some(Prompt::CommandPalette(prompt)) = self.mode.prompt() else {
            return;
        };
        let len = self.command_palette_entries_for(&prompt.input).len();
        if let Some(Prompt::CommandPalette(prompt)) = self.mode.prompt_mut() {
            prompt.selected.step(delta, len);
        }
    }

    pub(super) fn clamp_command_palette_selection(&mut self) {
        let Some(Prompt::CommandPalette(prompt)) = self.mode.prompt() else {
            return;
        };
        let len = self.command_palette_entries_for(&prompt.input).len();
        if let Some(Prompt::CommandPalette(prompt)) = self.mode.prompt_mut() {
            prompt.selected.clamp(len);
        }
    }

    pub fn begin_new_terminal_command(&mut self) -> bool {
        if self.selected_workspace_id().is_none() {
            return false;
        }

        self.open_prompt(Prompt::NewTerminalCommand(TerminalCommandPrompt {
            input: PromptInput::default(),
            error: None,
        }));
        true
    }

    pub fn cancel_prompt(&mut self) {
        self.close_prompt();
    }

    /// Whether the open prompt draws a list under it, where Ctrl+j/Ctrl+k move
    /// the selection. In every other prompt Ctrl+k is free, so it deletes to
    /// the end of the line there (E7).
    pub fn prompt_has_result_list(&self) -> bool {
        match self.mode.prompt() {
            Some(Prompt::CommandPalette(_)) => true,
            Some(Prompt::OpenWorkspace(prompt)) => {
                prompt.mode == OpenWorkspaceMode::ConfiguredProjects
            }
            _ => false,
        }
    }

    /// Open the keybinding overlay (E4).
    pub fn show_help(&mut self) {
        self.open_prompt(Prompt::Help(HelpPrompt::default()));
    }

    /// Scroll the overlay, clamped to its contents. The renderer clamps again
    /// against the rows that actually fit.
    pub fn scroll_help(&mut self, delta: isize) {
        let last_row = keybinding_help_rows().len().saturating_sub(1);
        if let Some(Prompt::Help(prompt)) = self.mode.prompt_mut() {
            prompt.scroll = prompt
                .scroll
                .saturating_add_signed(delta.clamp(-1024, 1024))
                .min(last_row);
        }
    }

    pub fn submit_new_terminal_command(&mut self) {
        let Some(Prompt::NewTerminalCommand(prompt)) = self.mode.prompt() else {
            return;
        };
        let command = prompt.input.trim().to_string();
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

        if let Some(terminal) =
            self.project
                .add_command_terminal(workspace, name.clone(), false, command)
        {
            self.close_prompt();
            self.select_item(NavItem::Terminal {
                workspace,
                terminal,
            });
            self.mark_structural_change();
        }
    }

    fn set_terminal_command_error(&mut self, message: impl Into<String>) {
        if let Some(Prompt::NewTerminalCommand(prompt)) = self.mode.prompt_mut() {
            prompt.error = Some(message.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::app::FocusMode;
    use crate::config::ConfiguredProject;

    #[test]
    fn command_terminal_prompt_adds_command_terminal() {
        let mut app = App::two_workspaces();
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
        assert_eq!(app.active_focus(), Some(FocusMode::Terminal));
        assert!(app.is_dirty());
    }

    #[test]
    fn command_palette_filters_and_returns_existing_actions() {
        let mut app = App::two_workspaces();

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

    #[test]
    fn the_help_overlay_opens_and_closes_like_any_other_prompt() {
        let mut app = App::two_workspaces();
        assert!(!app.is_prompt_active());

        app.show_help();
        assert!(app.is_prompt_active());
        assert!(matches!(app.prompt(), Some(Prompt::Help(_))));

        app.scroll_help(-5);
        let Some(Prompt::Help(prompt)) = app.prompt() else {
            panic!("help stays open while scrolling");
        };
        assert_eq!(prompt.scroll, 0);

        app.scroll_help(isize::MAX);
        let Some(Prompt::Help(prompt)) = app.prompt() else {
            panic!("help stays open while scrolling");
        };
        assert_eq!(prompt.scroll, keybinding_help_rows().len() - 1);

        app.cancel_prompt();
        assert_eq!(app.prompt(), None);
    }

    #[test]
    fn only_list_prompts_claim_ctrl_k_for_selection() {
        let mut app = App::two_workspaces();

        app.begin_command_palette();
        assert!(app.prompt_has_result_list());

        app.begin_open_workspace(&[]);
        assert!(!app.prompt_has_result_list());

        app.begin_open_workspace(&[ConfiguredProject {
            name: "mult".to_string(),
            path: PathBuf::from("/tmp"),
        }]);
        assert!(app.prompt_has_result_list());

        app.cancel_prompt();
        app.begin_new_terminal_command();
        assert!(!app.prompt_has_result_list());
    }
}
