//! `App`: everything the client keeps in memory about the session.
//!
//! The root owns the struct itself, its construction, the save/dirty flags, the
//! help overlay, and [`InteractionMode`] — the one field holding the two states
//! that cannot both be live, a modal prompt or a keyboard focus (F5). Focus is
//! derived from the selection rather than stored beside it, so a focus naming a
//! pane the sidebar is not on is unrepresentable.

mod bindings;
mod mutate;
mod nav;
mod notices;
mod open_workspace;
mod prompt;
mod search;
mod selection;

pub use self::bindings::*;
pub use self::mutate::*;
pub use self::nav::*;
pub use self::notices::*;
pub use self::open_workspace::*;
pub use self::prompt::*;
pub use self::search::*;
pub use self::selection::*;

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{
    ChatId, ChatStatus, ProjectState, TerminalId, WorkspaceId, DEFAULT_AGENT_CHAT_TITLE,
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub project: ProjectState,
    /// The currently selected sidebar item by identity, or `None` when there are
    /// no nav items. Stored as an identity (not a bare index) so it can never be
    /// an out-of-range position; the invariant "valid item or None" is kept by
    /// `reconcile_selection` after every structural change.
    selected: Option<NavItem>,
    /// The one field for the two states that cannot both be live: a modal
    /// prompt, or ordinary navigation with a keyboard focus. They used to be
    /// `prompt: Option<Prompt>` beside `focus: FocusMode`, so every call site
    /// had to re-check the first before trusting the second (F5).
    mode: InteractionMode,
    pub chat_buffers: BTreeMap<ChatId, ChatBuffer>,
    pub workspace_git_branches: BTreeMap<WorkspaceId, String>,
    pub active_search: Option<SearchState>,
    pub text_selection: Option<TextSelection>,
    pub should_quit: bool,
    dirty: bool,
    /// Set alongside `dirty` when the change altered the *structure* of the
    /// project — a workspace, chat or terminal added or removed — rather than
    /// only the contents of one. The runtime rate-limits ordinary content saves
    /// (B9) but never defers a structural one: they are rare, they are the
    /// changes a crash would most visibly lose, and they are what a restart
    /// reconstructs the session from. Cleared with `dirty` by `mark_saved`.
    structural_dirty: bool,
    recoverable_terminals: BTreeSet<TerminalId>,
    save_error: Option<String>,
    /// The transient status surface (E2). Everything that has no pane to be
    /// reported into — a daemon that will not connect, a protocol mismatch, a
    /// connection-wide `ServerMessage::Error` (B8), a rejected config, a state
    /// file that had to be reset — lands here instead of being attributed to a
    /// terminal that may not exist.
    notices: Vec<Notice>,
    /// Whether the keybinding overlay is up. Rendered over the frame, so it
    /// steals no space when it is down.
    help_visible: bool,
    /// Set by the "Reload config" palette action and taken by the event loop,
    /// which owns the `Config` (E9). The action itself cannot swap it: the
    /// handler only has `&Config`.
    config_reload_requested: bool,
}

/// Which surface the keyboard is talking to, as the renderer sees it.
///
/// Derived, never stored: `Chat` and `Terminal` come from what is selected, so
/// a focus can no longer name a pane the sidebar is not on (F5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusMode {
    #[default]
    Sidebar,
    Chat,
    Terminal,
}

/// Where the keyboard is while browsing.
///
/// Deliberately does not name *which* pane: that is the selection's business,
/// so `SelectedPane` cannot disagree with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BrowseFocus {
    #[default]
    Sidebar,
    SelectedPane,
}

/// The two mutually exclusive things the keyboard can be doing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InteractionMode {
    /// A modal prompt owns the keyboard. Nothing is focused while one is up —
    /// [`App::focus`] reports `None`, which is what the renderer already meant
    /// by `!is_prompt_active() && focus == …`. `resume` is the focus to go back
    /// to when the prompt closes; it is not observable while the prompt is up,
    /// but cancelling one has always returned the session exactly where it was.
    Prompting { prompt: Prompt, resume: BrowseFocus },
    /// Ordinary navigation.
    Browsing { focus: BrowseFocus },
}

impl Default for App {
    /// A fresh install's app: the first-run project, not an empty one.
    /// Production never uses this — `main` builds the `App` from the state
    /// `storage` loaded — so it exists for tests that want a populated tree.
    fn default() -> Self {
        Self::new(
            ProjectState::try_first_run()
                .expect("secure entropy is required to create project state"),
        )
    }
}

#[cfg(test)]
impl App {
    /// Test fixture wrapping [`ProjectState::seeded`] — an app populated with
    /// the historical agent-chat seed for tests that exercise chat behavior.
    pub(crate) fn seeded() -> Self {
        Self::new(ProjectState::seeded())
    }

    /// The open prompt, mutably. Tests drive prompts that production only
    /// reaches through a key handler (typing into an input, say), and the
    /// prompt is no longer a public field.
    pub(crate) fn prompt_mut_for_test(&mut self) -> Option<&mut Prompt> {
        self.prompt_mut()
    }
}

impl App {
    pub fn new(mut project: ProjectState) -> Self {
        // Durable schema/version/identity repair belongs exclusively to
        // storage. App construction only applies the existing presentation
        // normalization for historical agent titles.
        let titles_normalized = normalize_agent_chat_titles(&mut project);
        let chat_buffers = project
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter())
            .map(|chat| (chat.id, ChatBuffer::from_messages(&chat.messages)))
            .filter(|(_, buffer)| !buffer.is_empty())
            .collect();
        // A command loaded from disk with no restore intent must require a
        // deliberate start. This is what carries C1's no-relaunch rule across
        // client restarts: a command whose pane vanished is saved with
        // `restore_on_launch` clear, and is reconstructed here as
        // recovery-required on the next load.
        let recoverable_terminals = project
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter())
            .filter_map(|terminal| {
                (!terminal.restore_on_launch
                    && matches!(terminal.launch, crate::model::TerminalLaunch::Command(_)))
                .then_some(terminal.id)
            })
            .collect();
        let mut app = Self {
            project,
            selected: None,
            // Browsing from the first frame: nothing at startup opens a
            // prompt, and the restore-confirmation a recovered command
            // terminal needs is a per-terminal flag
            // (`terminal_requires_recovery`), not a modal surface. The focus is
            // replaced by `sync_focus_to_selection` below, once the selection
            // this app starts on is known.
            mode: InteractionMode::Browsing {
                focus: BrowseFocus::Sidebar,
            },
            chat_buffers,
            workspace_git_branches: BTreeMap::new(),
            active_search: None,
            text_selection: None,
            should_quit: false,
            dirty: titles_normalized,
            structural_dirty: false,
            recoverable_terminals,
            save_error: None,
            notices: Vec::new(),
            help_visible: false,
            config_reload_requested: false,
        };
        app.reconcile_selection(None);
        app.sync_focus_to_selection();
        app
    }

    /// The open prompt, if any.
    pub fn prompt(&self) -> Option<&Prompt> {
        match &self.mode {
            InteractionMode::Prompting { prompt, .. } => Some(prompt),
            InteractionMode::Browsing { .. } => None,
        }
    }

    fn prompt_mut(&mut self) -> Option<&mut Prompt> {
        match &mut self.mode {
            InteractionMode::Prompting { prompt, .. } => Some(prompt),
            InteractionMode::Browsing { .. } => None,
        }
    }

    /// Open `prompt`, remembering the focus to resume when it closes. Opening a
    /// second prompt over the first keeps the *original* focus to resume, which
    /// is what a chain of prompts did when focus was a field of its own.
    fn set_prompt(&mut self, prompt: Prompt) {
        let resume = self.browse_focus();
        self.mode = InteractionMode::Prompting { prompt, resume };
    }

    fn clear_prompt(&mut self) {
        self.mode = InteractionMode::Browsing {
            focus: self.browse_focus(),
        };
    }

    fn take_prompt(&mut self) -> Option<Prompt> {
        let focus = self.browse_focus();
        match std::mem::replace(&mut self.mode, InteractionMode::Browsing { focus }) {
            InteractionMode::Prompting { prompt, .. } => Some(prompt),
            InteractionMode::Browsing { .. } => None,
        }
    }

    /// The live keyboard focus, or `None` while a prompt owns the keyboard.
    ///
    /// `SelectedPane` resolves through the selection, so a pane focus with
    /// nothing selected is reported as the sidebar rather than as a pane that
    /// is not there.
    pub fn focus(&self) -> Option<FocusMode> {
        match &self.mode {
            InteractionMode::Prompting { .. } => None,
            InteractionMode::Browsing { focus } => Some(match focus {
                BrowseFocus::Sidebar => FocusMode::Sidebar,
                BrowseFocus::SelectedPane => {
                    self.selected_main_focus().unwrap_or(FocusMode::Sidebar)
                }
            }),
        }
    }

    fn browse_focus(&self) -> BrowseFocus {
        match &self.mode {
            InteractionMode::Prompting { resume, .. } => *resume,
            InteractionMode::Browsing { focus } => *focus,
        }
    }

    /// Move the browsing focus without disturbing an open prompt.
    ///
    /// A PTY that exits while the palette is up still re-derives the focus it
    /// will return to (`end_pty_input`), exactly as the old standalone field
    /// did; it must not close the prompt to do so.
    fn set_browse_focus(&mut self, focus: BrowseFocus) {
        match &mut self.mode {
            InteractionMode::Prompting { resume, .. } => *resume = focus,
            InteractionMode::Browsing { focus: current } => *current = focus,
        }
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn cancel_quit(&mut self) {
        self.should_quit = false;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the unsaved changes include a structural one. See
    /// [`App::structural_dirty`].
    pub fn has_structural_change(&self) -> bool {
        self.structural_dirty
    }

    /// Record a change that added or removed a workspace, chat or terminal.
    fn mark_structural_change(&mut self) {
        self.dirty = true;
        self.structural_dirty = true;
    }

    pub fn mark_clean(&mut self) {
        self.mark_saved();
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
        self.structural_dirty = false;
        self.save_error = None;
        // The save-failure notice describes a condition that has just ended.
        self.notices
            .retain(|notice| notice.source != NoticeSource::SaveFailure);
    }

    pub fn is_help_visible(&self) -> bool {
        self.help_visible
    }

    pub fn show_help(&mut self) {
        self.help_visible = true;
    }

    pub fn hide_help(&mut self) -> bool {
        let was_visible = self.help_visible;
        self.help_visible = false;
        was_visible
    }

    /// Whether a bare `?` should open the overlay rather than being typed.
    ///
    /// Input reaches a selected chat/terminal by being typed at it — there is no
    /// input mode to leave — so a global `?` would swallow a character every
    /// running PTY has a use for. It therefore only opens help when no pane
    /// would receive it; `F1` is unconditional because no shell binds it.
    pub fn help_key_opens_help(&self) -> bool {
        self.pty_input_target().is_none()
    }

    /// Ask the event loop to re-read `config.json` (E9). The palette handler
    /// only holds `&Config`, so the swap happens where the `Config` is owned.
    pub fn request_config_reload(&mut self) {
        self.config_reload_requested = true;
    }

    pub fn take_config_reload_request(&mut self) -> bool {
        std::mem::take(&mut self.config_reload_requested)
    }

    pub fn is_prompt_active(&self) -> bool {
        self.prompt().is_some()
    }

    pub fn workspace_git_branch(&self, workspace: WorkspaceId) -> Option<&str> {
        self.workspace_git_branches
            .get(&workspace)
            .map(String::as_str)
    }

    pub fn replace_workspace_git_branches(
        &mut self,
        branches: impl IntoIterator<Item = (WorkspaceId, Option<String>)>,
    ) -> bool {
        let next = branches
            .into_iter()
            .filter_map(|(workspace, branch)| {
                let branch = clean_git_branch_name(branch?)?;
                Some((workspace, branch))
            })
            .collect::<BTreeMap<_, _>>();

        if self.workspace_git_branches == next {
            return false;
        }

        self.workspace_git_branches = next;
        true
    }

    pub fn focus_sidebar(&mut self) {
        self.set_browse_focus(BrowseFocus::Sidebar);
    }

    pub fn focus_selected_main(&mut self) -> bool {
        if self.selected_main_focus().is_none() {
            return false;
        }

        self.set_browse_focus(BrowseFocus::SelectedPane);
        true
    }

    fn selected_main_focus(&self) -> Option<FocusMode> {
        match self.selected_item()? {
            NavItem::Chat { .. } => Some(FocusMode::Chat),
            NavItem::Terminal { .. } => Some(FocusMode::Terminal),
        }
    }

    fn sync_focus_to_selection(&mut self) {
        self.set_browse_focus(if self.selected_main_focus().is_some() {
            BrowseFocus::SelectedPane
        } else {
            BrowseFocus::Sidebar
        });
        // Every selection change funnels through here (keyboard nav, mouse,
        // startup, post-delete reconcile), so it is the single place to record
        // that the user is now looking at the selected item: a finished agent
        // the user navigates onto stops being an unseen notification.
        self.mark_selected_done_seen();
    }

    /// Marks the currently selected chat's finished state as seen. A no-op for
    /// any other status or when a terminal is selected.
    ///
    /// The bit lives in [`ChatStatus`] itself, so this is the only place that
    /// can set it and there is no second table to fall out of step (F16).
    fn mark_selected_done_seen(&mut self) {
        let Some((workspace, chat)) = self.selected_chat_id() else {
            return;
        };
        let Some(session) = self.project.chat_mut(workspace, chat) else {
            return;
        };
        if session.status.is_done() && !session.status.done_seen() {
            session.status = ChatStatus::DoneSeen;
            self.dirty = true;
        }
    }

    /// Whether the chat has finished and the user has already looked at it.
    /// The renderer uses this to choose between the green "finished"
    /// notification and the gray inactive icon.
    pub fn chat_done_seen(&self, chat: ChatId) -> bool {
        self.project
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter())
            .find(|session| session.id == chat)
            .is_some_and(|session| session.status.done_seen())
    }

    pub fn begin_terminal_input(&mut self) -> bool {
        if self.selected_terminal_id().is_none() {
            return false;
        }

        self.set_browse_focus(BrowseFocus::SelectedPane);
        true
    }

    pub fn begin_chat_agent_input(&mut self) -> bool {
        if self.selected_chat_id().is_none() {
            return false;
        }

        self.set_browse_focus(BrowseFocus::SelectedPane);
        true
    }

    pub fn end_pty_input(&mut self) {
        self.sync_focus_to_selection();
    }
}

fn normalize_agent_chat_titles(project: &mut ProjectState) -> bool {
    let mut changed = false;
    for chat in project
        .workspaces
        .iter_mut()
        .flat_map(|workspace| workspace.chats.iter_mut())
    {
        if chat.name != DEFAULT_AGENT_CHAT_TITLE {
            chat.name = DEFAULT_AGENT_CHAT_TITLE.to_string();
            changed = true;
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_background_agent_notification_clears_once_seen() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        // chats[0] is selected by default; chats[1] finishes in the background.
        let foreground = app.project.workspaces[0].chats[0].id;
        let background = app.project.workspaces[0].chats[1].id;

        // Finishing off-screen arms the green "finished" notification (unseen).
        assert!(app.mark_chat_status_by_id(background, ChatStatus::Done));
        assert!(!app.chat_done_seen(background));

        // Navigating onto the finished agent marks it seen -> gray.
        app.select_item(NavItem::Chat {
            workspace,
            chat: background,
        });
        assert!(app.chat_done_seen(background));

        // Navigating away keeps it gray indefinitely; it does not re-arm green.
        app.select_item(NavItem::Chat {
            workspace,
            chat: foreground,
        });
        assert!(app.chat_done_seen(background));

        // Re-prompting the agent (back to running) re-arms the notification so
        // the next finish is shown again.
        assert!(app.mark_chat_status_by_id(background, ChatStatus::Thinking));
        assert!(!app.chat_done_seen(background));
    }

    #[test]
    fn agent_finishing_while_selected_is_never_an_unseen_notification() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let selected = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat {
            workspace,
            chat: selected,
        });

        // Finishing while the user is already looking at the agent counts as
        // seen at once, so the icon never flashes green for the watched agent.
        assert!(app.mark_chat_status_by_id(selected, ChatStatus::Done));
        assert!(app.chat_done_seen(selected));
    }

    #[test]
    fn app_normalizes_chat_titles_to_agent_on_load() {
        let mut state = ProjectState::seeded();
        state.workspaces[0].chats[0].name = "pi: old topic title".to_string();
        let app = App::new(state);

        assert_eq!(
            app.project.workspaces[0].chats[0].name,
            DEFAULT_AGENT_CHAT_TITLE
        );
        assert!(app.is_dirty());
    }

    #[test]
    fn app_preserves_storage_owned_status_and_allocator_state() {
        let mut state = ProjectState::seeded();
        state.workspaces[0].chats[0].status = ChatStatus::Thinking;
        state.next_workspace_id = 1;
        state.next_chat_id = 1;
        state.next_terminal_id = 1;

        let app = App::new(state);

        assert_eq!(
            app.project.workspaces[0].chats[0].status,
            ChatStatus::Thinking
        );
        assert_eq!(app.project.next_workspace_id, 1);
        assert_eq!(app.project.next_chat_id, 1);
        assert_eq!(app.project.next_terminal_id, 1);
        assert!(!app.is_dirty());
    }

    #[test]
    fn workspace_git_branches_are_runtime_only() {
        let mut app = App::default();
        app.mark_clean();
        let workspace = app.project.workspaces[0].id;

        assert!(app.replace_workspace_git_branches([(workspace, Some(" main ".to_string()))]));

        assert_eq!(app.workspace_git_branch(workspace), Some("main"));
        assert!(!app.is_dirty());
    }

    /// B9: content changes may be batched by the runtime's save rate limit, but
    /// adding or removing a workspace/chat/terminal must be flagged so the save
    /// happens immediately.
    #[test]
    fn structural_changes_are_flagged_and_content_changes_are_not() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.mark_clean();
        assert!(!app.has_structural_change());

        app.apply_agent_event(crate::agent::AgentEvent::MessageDelta {
            target: crate::agent::AgentTarget { workspace, chat },
            role: crate::agent::AgentMessageRole::Assistant,
            text: "streamed".to_string(),
        });
        assert!(app.is_dirty());
        assert!(
            !app.has_structural_change(),
            "a streamed delta is content, not structure"
        );

        app.add_terminal_to_selected_workspace();
        assert!(app.has_structural_change());

        app.mark_saved();
        assert!(!app.has_structural_change());

        // Deleting is structural too, in both directions.
        assert!(app.begin_delete_selected());
        assert!(!app.confirm_delete().is_empty());
        assert!(app.has_structural_change());
    }

    #[test]
    fn the_help_overlay_opens_and_closes() {
        let mut app = App::default();
        assert!(!app.is_help_visible());

        app.show_help();
        assert!(app.is_help_visible());
        assert!(app.hide_help());
        assert!(!app.hide_help());
    }

    #[test]
    fn a_bare_question_mark_only_opens_help_when_no_pane_would_receive_it() {
        let mut app = App::default();
        // The seed state selects a terminal, which takes every ordinary key.
        assert!(app.pty_input_target().is_some());
        assert!(!app.help_key_opens_help());

        app.project.workspaces.clear();
        app.select_nav_index(0);
        assert!(app.pty_input_target().is_none());
        assert!(app.help_key_opens_help());
    }

    #[test]
    fn a_config_reload_request_is_taken_exactly_once() {
        let mut app = App::default();
        assert!(!app.take_config_reload_request());

        app.request_config_reload();
        assert!(app.take_config_reload_request());
        assert!(!app.take_config_reload_request());
    }

    // ---- F5: one field for prompt-or-focus ---------------------------------

    #[test]
    fn a_prompt_owns_the_keyboard_and_gives_the_focus_back_untouched() {
        let mut app = App::default();
        app.focus_sidebar();
        assert_eq!(app.focus(), Some(FocusMode::Sidebar));

        app.begin_command_palette();
        assert_eq!(
            app.focus(),
            None,
            "nothing is focused while a prompt is modal"
        );
        assert!(app.is_prompt_active());

        app.cancel_prompt();
        assert_eq!(
            app.focus(),
            Some(FocusMode::Sidebar),
            "cancelling a prompt returns the focus the session had"
        );
    }

    #[test]
    fn focus_follows_the_selection_instead_of_being_stored_beside_it() {
        let mut app = App::default();
        let terminal = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Terminal { .. }))
            .expect("seed state has a terminal");
        app.select_nav_index(terminal);
        assert!(app.focus_selected_main());
        assert_eq!(app.focus(), Some(FocusMode::Terminal));

        let mut chat_app = App::seeded();
        let chat = chat_app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Chat { .. }))
            .expect("seed state has a chat");
        chat_app.select_nav_index(chat);
        assert!(chat_app.focus_selected_main());
        assert_eq!(
            chat_app.focus(),
            Some(FocusMode::Chat),
            "the same pane focus reads as Chat purely because a chat is selected"
        );
    }

    /// A PTY exiting is not a reason to close the palette. `end_pty_input` runs
    /// from the event loop whatever the user happens to have open, so it moves
    /// the focus the prompt will resume into, not the live one.
    #[test]
    fn a_pty_exiting_under_an_open_prompt_leaves_the_prompt_alone() {
        let mut app = App::default();
        app.focus_sidebar();
        app.begin_command_palette();

        app.end_pty_input();

        assert!(
            matches!(app.prompt(), Some(Prompt::CommandPalette(_))),
            "the prompt survives"
        );
        app.cancel_prompt();
        assert_eq!(
            app.focus(),
            Some(FocusMode::Terminal),
            "and it resumes into the focus `end_pty_input` re-derived"
        );
    }
}
