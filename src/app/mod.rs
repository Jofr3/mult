//! Client-side UI state: what is selected, what the keyboard is doing, and
//! everything the renderer reads that is not the durable project.
//!
//! The state itself lives here; the behaviour is split by concern — [`nav`]
//! (the sidebar order and the selection that walks it), [`delete`] (removing
//! what is selected), [`prompt`] and [`open_workspace`] (the modal prompts),
//! [`text_input`] (the editing every prompt shares), [`search`], [`selection`]
//! (the mouse text selection), [`status`] (the status-line queue) and
//! [`bindings`] (the one keybinding/command table).

pub mod bindings;
pub mod delete;
pub mod nav;
pub mod open_workspace;
pub mod prompt;
pub mod search;
pub mod selection;
pub mod status;
pub mod text_input;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub use self::{
    bindings::{
        keybinding_help_rows, Availability, Binding, CommandAction, CommandPaletteEntry, HelpRow,
        HelpSection, BINDINGS,
    },
    delete::{DeleteRequest, DeleteTarget},
    nav::{NavItem, SidebarRow},
    open_workspace::{OpenWorkspaceMatch, OpenWorkspaceMode, OpenWorkspacePrompt},
    prompt::{
        CommandPalettePrompt, ConfirmDeletePrompt, ConfirmRestorePrompt, HelpPrompt,
        PendingRestore, Prompt, SearchPrompt, TerminalCommandPrompt,
    },
    search::{SearchScope, SearchState},
    selection::{SelectionCell, TextSelection, TextSelectionRange},
    status::{StatusLevel, StatusNotice},
    text_input::{ListSelection, PromptInput},
};

use crate::model::{
    AgentKind, ChatId, ChatStatus, ProjectState, TerminalId, TerminalLaunch, WorkspaceId,
    DEFAULT_AGENT_CHAT_TITLE, STATE_VERSION,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub project: ProjectState,
    /// The currently selected sidebar item by identity, or `None` when there are
    /// no nav items. Stored as an identity (not a bare index) so it can never be
    /// an out-of-range position; the invariant "valid item or None" is kept by
    /// `reconcile_selection` after every structural change.
    selected: Option<NavItem>,
    /// The one interaction the keyboard is in (F5).
    ///
    /// A prompt and a pane focus used to be two independent optional fields, so
    /// `focus` was meaningless while a prompt was open and every call site
    /// re-checked `is_prompt_active()` for itself; nothing stopped a `Chat`
    /// focus while a terminal was selected except a manually sprinkled
    /// `sync_focus_to_selection`. One field makes both combinations
    /// unrepresentable, and the *pane* half of the focus is derived from
    /// `selected` rather than stored.
    mode: InteractionMode,
    pub workspace_git_branches: BTreeMap<WorkspaceId, String>,
    pub active_search: Option<SearchState>,
    pub text_selection: Option<TextSelection>,
    pub should_quit: bool,
    dirty: bool,
    /// Set alongside `dirty` by changes whose loss the user would see as a
    /// missing workspace, chat or terminal, as opposed to streamed chat text.
    /// The rate-limited save in the runtime loop flushes immediately when this
    /// is set, so a structural edit is never more than one tick from disk.
    save_urgently: bool,
    /// Non-fatal problems the user has not dismissed yet, oldest first.
    ///
    /// Everything without a pane of its own ends up here: a save that could not
    /// be written, a frame that could not be drawn, a daemon that could not be
    /// reached, the config warnings from startup, the state-backup notice. The
    /// daemon-connection message used to be queued against
    /// `PtyKey::Terminal(TerminalId(0))` — an id that cannot exist, since
    /// terminal ids start at 1 — so a user with no `mult-server` saw an inert
    /// UI and no explanation at all (E2). Bounded, deduplicated, and rendered
    /// one at a time so a repeated per-tick failure cannot flood it or take the
    /// screen.
    status_notices: VecDeque<StatusNotice>,
    /// Set by the "Reload config" command; the event loop owns the `Config`, so
    /// it does the swap and clears this (E9).
    config_reload_requested: bool,
    /// Something the user would see has changed, so the loop owes them a frame.
    ///
    /// Queuing a status notice sets this, which is why it exists: the loop only
    /// recomputes the layout and redraws inside its `needs_redraw` branch, and
    /// the two producers that mutated the notice queue without asking for a
    /// frame — the failed save and the failed clipboard write — left a status
    /// line that was never drawn until an unrelated keypress happened to trigger
    /// one (F11). The loop drains it once per tick, so a new notice producer
    /// cannot forget.
    redraw_requested: bool,
    /// Command terminals whose command line came out of `state.json` and has
    /// not been approved to run in this session (C1).
    ///
    /// `state.json` is an execution boundary: for a `Command` terminal the
    /// program is the *file's* choice, not `$SHELL`, so no automatic path may
    /// launch one — not the auto-start tick, not a key pressed at a focused
    /// pane. Declining the restore prompt used to only drop the prompt, so the
    /// very next tick auto-started exactly what the user had just refused; a
    /// planted terminal with `restore_on_launch: false` was never even asked
    /// about and auto-started with no prompt at all. Approval is the prompt's
    /// `y` or the explicit "Start selected PTY" command, and both clear the
    /// entry. Terminals created in this session are the user's own typing and
    /// are never in here.
    unapproved_command_terminals: BTreeSet<TerminalId>,
    /// Text a copy action has queued for the system clipboard, waiting to be
    /// emitted as OSC 52.
    ///
    /// The escape used to be written straight to `io::stdout()` from the mouse
    /// handler, i.e. in the middle of the event loop and outside the frame the
    /// renderer was about to draw — two writers on one terminal. Queuing it
    /// here lets the loop emit it through the terminal's own output handle,
    /// after a frame completes.
    pending_clipboard: Option<String>,
}

/// The mutually exclusive states the keyboard can be in.
///
/// A prompt is modal: while one is open no key reaches a pane, which is why the
/// browsing focus is *suspended* here rather than sitting in a field the
/// renderer might read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionMode {
    /// A prompt owns every key. `resume` is the browsing focus it suspended,
    /// restored verbatim when the prompt closes.
    Prompting { prompt: Prompt, resume: PaneFocus },
    /// Keys go to the sidebar, or to the selected pane's PTY.
    Browsing { focus: PaneFocus },
}

/// Which half of the frame has the keyboard while browsing.
///
/// Deliberately two-valued: *which* pane is focused is not stored, it is the
/// selected item, so a `Chat` focus with a terminal selected cannot be written
/// down. [`App::active_focus`] resolves the pair into a [`FocusMode`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaneFocus {
    #[default]
    Sidebar,
    /// The selected chat or terminal.
    Pane,
}

/// What the renderer draws as focused: the sidebar, or the selected pane by
/// kind. Derived from [`InteractionMode`] plus the selection, never stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusMode {
    #[default]
    Sidebar,
    Chat,
    Terminal,
}

impl Default for InteractionMode {
    fn default() -> Self {
        Self::Browsing {
            focus: PaneFocus::default(),
        }
    }
}

impl InteractionMode {
    fn prompt(&self) -> Option<&Prompt> {
        match self {
            Self::Prompting { prompt, .. } => Some(prompt),
            Self::Browsing { .. } => None,
        }
    }

    fn prompt_mut(&mut self) -> Option<&mut Prompt> {
        match self {
            Self::Prompting { prompt, .. } => Some(prompt),
            Self::Browsing { .. } => None,
        }
    }

    /// The browsing focus, whether it is live or suspended behind a prompt.
    fn focus(&self) -> PaneFocus {
        match self {
            Self::Prompting { resume, .. } => *resume,
            Self::Browsing { focus } => *focus,
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new(ProjectState::default())
    }
}

#[cfg(test)]
impl App {
    /// Test fixture wrapping [`ProjectState::two_workspaces`], which is what
    /// `App::two_workspaces()` used to produce before the demo seed was removed (F12).
    pub(crate) fn two_workspaces() -> Self {
        Self::new(ProjectState::two_workspaces())
    }

    /// Test fixture wrapping [`ProjectState::seeded`] — an app populated with
    /// agent chats, for tests that exercise chat behavior.
    pub(crate) fn seeded() -> Self {
        Self::new(ProjectState::seeded())
    }
}

impl App {
    pub fn new(mut project: ProjectState) -> Self {
        let version_normalized = project.version != STATE_VERSION;
        project.version = STATE_VERSION;
        let ids_normalized = project.normalize_next_ids();
        let titles_normalized = normalize_agent_chat_titles(&mut project);
        let chat_statuses_normalized = normalize_transient_chat_statuses(&mut project);
        // Every command terminal that arrives with the project came out of
        // `state.json`, so none of them may run before the user has said so
        // (C1). See `unapproved_command_terminals`.
        let unapproved_command_terminals = project
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter())
            .filter(|terminal| matches!(terminal.launch, TerminalLaunch::Command(_)))
            .map(|terminal| terminal.id)
            .collect();
        let mut app = Self {
            project,
            selected: None,
            mode: InteractionMode::default(),
            workspace_git_branches: BTreeMap::new(),
            active_search: None,
            text_selection: None,
            should_quit: false,
            dirty: version_normalized
                || ids_normalized
                || titles_normalized
                || chat_statuses_normalized,
            // A repaired state file is worth writing back promptly: the repair
            // renumbered ids that the rest of the session now depends on.
            save_urgently: version_normalized
                || ids_normalized
                || titles_normalized
                || chat_statuses_normalized,
            status_notices: VecDeque::new(),
            config_reload_requested: false,
            redraw_requested: false,
            unapproved_command_terminals,
            pending_clipboard: None,
        };
        app.reconcile_selection(None);
        app.sync_focus_to_selection();
        app
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// This project's daemon session namespace, allocated on first use (A3).
    /// Allocating one is a change worth saving promptly: until it is persisted,
    /// a restart would take a *new* namespace and abandon its own live panes.
    pub fn ensure_instance_token(&mut self) -> u64 {
        if self.project.ensure_instance() {
            self.dirty = true;
            self.save_urgently = true;
        }
        self.project.instance
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
        self.save_urgently = false;
    }

    /// Whether the pending change should bypass the save rate limit.
    pub fn needs_urgent_save(&self) -> bool {
        self.save_urgently
    }

    /// Mark a structural change (workspace/chat/terminal added or removed).
    fn mark_structural_change(&mut self) {
        self.dirty = true;
        self.save_urgently = true;
    }

    /// Ask the event loop to re-read `config.json` and swap it in (E9).
    pub fn request_config_reload(&mut self) {
        self.config_reload_requested = true;
    }

    /// Take a pending reload request, if one was made since the last call.
    pub fn take_config_reload_request(&mut self) -> bool {
        std::mem::take(&mut self.config_reload_requested)
    }

    /// Ask the loop for a frame. Set by anything that changes what is on screen
    /// without going through the event handler — a queued status notice, today
    /// (F11).
    fn request_redraw(&mut self) {
        self.redraw_requested = true;
    }

    /// Take a pending redraw request, if one was made since the last call.
    pub fn take_redraw_request(&mut self) -> bool {
        std::mem::take(&mut self.redraw_requested)
    }

    /// Queue `text` for the system clipboard, replacing anything not yet
    /// emitted (only the latest copy is meaningful).
    pub fn queue_clipboard_text(&mut self, text: impl Into<String>) {
        self.pending_clipboard = Some(text.into());
    }

    /// Take the queued clipboard text, if any.
    pub fn take_pending_clipboard(&mut self) -> Option<String> {
        self.pending_clipboard.take()
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

    /// The open prompt, if any. The only way to observe one: a prompt and a
    /// browsing focus cannot both be live, so there is no field to read.
    pub fn prompt(&self) -> Option<&Prompt> {
        self.mode.prompt()
    }

    /// Mutable access to the open prompt, for tests that need to seed a
    /// prompt's input directly. Production code edits a prompt through the
    /// typed methods below.
    #[cfg(test)]
    pub(crate) fn prompt_mut(&mut self) -> Option<&mut Prompt> {
        self.mode.prompt_mut()
    }

    /// Open `prompt`, suspending (not discarding) the browsing focus.
    fn open_prompt(&mut self, prompt: Prompt) {
        self.mode = InteractionMode::Prompting {
            prompt,
            resume: self.mode.focus(),
        };
    }

    /// Close whatever prompt is open, resuming the focus it suspended.
    fn close_prompt(&mut self) {
        self.mode = InteractionMode::Browsing {
            focus: self.mode.focus(),
        };
    }

    /// Close the open prompt and hand it back.
    fn take_prompt(&mut self) -> Option<Prompt> {
        let resumed = InteractionMode::Browsing {
            focus: self.mode.focus(),
        };
        match std::mem::replace(&mut self.mode, resumed) {
            InteractionMode::Prompting { prompt, .. } => Some(prompt),
            InteractionMode::Browsing { .. } => None,
        }
    }

    pub fn is_prompt_active(&self) -> bool {
        self.mode.prompt().is_some()
    }

    /// What the renderer should draw as focused, or `None` while a prompt owns
    /// the keyboard — the check every call site used to make for itself (F5).
    ///
    /// A `Pane` focus with nothing selected resolves to `None` rather than to
    /// the sidebar: the sidebar is focused only when it was actually chosen.
    pub fn active_focus(&self) -> Option<FocusMode> {
        match &self.mode {
            InteractionMode::Prompting { .. } => None,
            InteractionMode::Browsing {
                focus: PaneFocus::Sidebar,
            } => Some(FocusMode::Sidebar),
            InteractionMode::Browsing {
                focus: PaneFocus::Pane,
            } => self.selected_main_focus(),
        }
    }

    /// Set the browsing focus, writing through a prompt to the focus it will
    /// resume with — the field this replaced was updated the same way.
    fn set_browsing_focus(&mut self, focus: PaneFocus) {
        match &mut self.mode {
            InteractionMode::Prompting { resume, .. } => *resume = focus,
            InteractionMode::Browsing { focus: current } => *current = focus,
        }
    }

    /// Record whether this terminal should come back at the next launch.
    ///
    /// The two callers are "it just started" and "it just stopped", which is the
    /// same pair of sites that used to write a persisted `TerminalStatus`. The
    /// difference is what the value now means: nothing reads it to decide how a
    /// terminal is *drawn* — that comes from `PtyRuntime` — so a missed call can
    /// only cost a terminal its restore, never leave one rendered live forever
    /// (F16).
    pub fn set_terminal_restore_on_launch(&mut self, terminal: TerminalId, restore: bool) {
        if let Some(terminal) = self.project.terminal_mut_by_id(terminal) {
            if terminal.restore_on_launch != restore {
                terminal.restore_on_launch = restore;
                self.dirty = true;
            }
        }
    }

    /// Returns whether the chat's status actually changed (useful for deciding
    /// if a redraw is needed).
    /// Apply an externally-observed status to a chat.
    ///
    /// The seen bit is decided here rather than by the caller, and only when the
    /// state itself changes: a chat that finishes while the user is already
    /// looking at it counts as seen immediately (so it never flashes green),
    /// finishing in the background arms the notification, and leaving `Done`
    /// discards the bit with the state it belonged to. The status source repeats
    /// itself every poll, so `Done` arriving again for a chat already marked
    /// seen must *not* re-arm the notification — hence the comparison is on the
    /// state, not on the whole value (F16).
    pub fn mark_chat_status_by_id(&mut self, chat: ChatId, status: ChatStatus) -> bool {
        let selected = self.selected_chat_id().map(|(_, chat)| chat) == Some(chat);
        let mut changed = false;
        for chat_session in self
            .project
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.chats.iter_mut())
        {
            if chat_session.id == chat {
                if !chat_session.status.is_same_state(status) {
                    chat_session.status = match status {
                        ChatStatus::Done { .. } => ChatStatus::Done { seen: selected },
                        status => status,
                    };
                    self.dirty = true;
                    changed = true;
                }
                break;
            }
        }
        changed
    }

    pub fn add_chat_to_selected_workspace_and_return(
        &mut self,
        agent: AgentKind,
    ) -> Option<(WorkspaceId, ChatId)> {
        let workspace = self.selected_workspace_id()?;
        let name = DEFAULT_AGENT_CHAT_TITLE.to_string();
        let chat = self
            .project
            .add_chat(workspace, name, ChatStatus::Idle, agent)?;
        self.select_item(NavItem::Chat { workspace, chat });
        self.mark_structural_change();
        Some((workspace, chat))
    }

    pub fn add_terminal_to_selected_workspace(&mut self) {
        let Some(workspace) = self.selected_workspace_id() else {
            return;
        };
        let next = self
            .project
            .workspace(workspace)
            .map(|workspace| workspace.terminals.len() + 1)
            .unwrap_or(1);

        let name = format!("terminal-{next}");
        if let Some(terminal) = self.project.add_terminal(workspace, name.clone(), false) {
            self.select_item(NavItem::Terminal {
                workspace,
                terminal,
            });
            self.mark_structural_change();
        }
    }
}

fn clean_git_branch_name(branch: String) -> Option<String> {
    let branch = branch.trim();
    (!branch.is_empty()).then(|| branch.to_string())
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

fn normalize_transient_chat_statuses(project: &mut ProjectState) -> bool {
    let mut changed = false;
    for chat in project
        .workspaces
        .iter_mut()
        .flat_map(|workspace| workspace.chats.iter_mut())
    {
        if matches!(chat.status, ChatStatus::Thinking | ChatStatus::Waiting) {
            chat.status = ChatStatus::Idle;
            changed = true;
        }
    }

    changed
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
    fn finished_background_agent_notification_clears_once_seen() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        // chats[0] is selected by default; chats[1] finishes in the background.
        let foreground = app.project.workspaces[0].chats[0].id;
        let background = app.project.workspaces[0].chats[1].id;

        // Finishing off-screen arms the green "finished" notification (unseen).
        assert!(app.mark_chat_status_by_id(background, ChatStatus::Done { seen: false }));
        assert_eq!(
            chat_status(&app, background),
            ChatStatus::Done { seen: false }
        );

        // Navigating onto the finished agent marks it seen -> gray.
        app.select_item(NavItem::Chat {
            workspace,
            chat: background,
        });
        assert_eq!(
            chat_status(&app, background),
            ChatStatus::Done { seen: true }
        );

        // The status source repeats itself every poll; `Done` arriving again
        // for a chat already seen must not re-arm the notification (F16).
        assert!(!app.mark_chat_status_by_id(background, ChatStatus::Done { seen: false }));
        assert_eq!(
            chat_status(&app, background),
            ChatStatus::Done { seen: true }
        );

        // Navigating away keeps it gray indefinitely; it does not re-arm green.
        app.select_item(NavItem::Chat {
            workspace,
            chat: foreground,
        });
        assert_eq!(
            chat_status(&app, background),
            ChatStatus::Done { seen: true }
        );

        // Re-prompting the agent (back to running) re-arms the notification so
        // the next finish is shown again.
        assert!(app.mark_chat_status_by_id(background, ChatStatus::Thinking));
        assert!(app.mark_chat_status_by_id(background, ChatStatus::Done { seen: false }));
        assert_eq!(
            chat_status(&app, background),
            ChatStatus::Done { seen: false }
        );
    }

    fn chat_status(app: &App, chat: ChatId) -> ChatStatus {
        app.project
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter())
            .find(|session| session.id == chat)
            .expect("chat exists")
            .status
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
        assert!(app.mark_chat_status_by_id(selected, ChatStatus::Done { seen: false }));
        assert_eq!(chat_status(&app, selected), ChatStatus::Done { seen: true });
    }

    #[test]
    fn new_chats_use_agent_title() {
        let mut app = App::two_workspaces();
        let Some((workspace, chat)) = app.add_chat_to_selected_workspace_and_return(AgentKind::Pi)
        else {
            panic!("chat should be added");
        };

        assert_eq!(
            app.project.chat(workspace, chat).unwrap().name,
            DEFAULT_AGENT_CHAT_TITLE
        );
    }

    #[test]
    fn new_chats_record_their_agent_kind() {
        let mut app = App::two_workspaces();
        let Some((workspace, chat)) =
            app.add_chat_to_selected_workspace_and_return(AgentKind::ClaudeCode)
        else {
            panic!("chat should be added");
        };

        assert_eq!(
            app.project.chat(workspace, chat).unwrap().agent,
            AgentKind::ClaudeCode
        );
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
    fn app_normalizes_transient_chat_statuses_on_load() {
        let mut state = ProjectState::seeded();
        state.workspaces[0].chats[0].status = ChatStatus::Thinking;
        state.workspaces[0].chats[1].status = ChatStatus::Waiting;
        state.workspaces[1].chats[0].status = ChatStatus::Done { seen: false };

        let app = App::new(state);

        assert_eq!(app.project.workspaces[0].chats[0].status, ChatStatus::Idle);
        assert_eq!(app.project.workspaces[0].chats[1].status, ChatStatus::Idle);
        assert_eq!(
            app.project.workspaces[1].chats[0].status,
            ChatStatus::Done { seen: false }
        );
        assert!(app.is_dirty());
    }

    #[test]
    fn app_repairs_low_allocators_on_load() {
        let state = ProjectState {
            next_workspace_id: 1,
            next_chat_id: 1,
            next_terminal_id: 1,
            ..ProjectState::seeded()
        };

        let app = App::new(state);

        assert_eq!(app.project.next_workspace_id, 3);
        assert_eq!(app.project.next_chat_id, 4);
        assert_eq!(app.project.next_terminal_id, 3);
        assert!(app.is_dirty());
    }

    #[test]
    fn workspace_git_branches_are_runtime_only() {
        let mut app = App::two_workspaces();
        app.mark_clean();
        let workspace = app.project.workspaces[0].id;

        assert!(app.replace_workspace_git_branches([(workspace, Some(" main ".to_string()))]));

        assert_eq!(app.workspace_git_branch(workspace), Some("main"));
        assert!(!app.is_dirty());
    }

    #[test]
    fn unchanged_status_updates_do_not_mark_dirty() {
        let mut app = App::seeded();
        let terminal = app.project.workspaces[0].terminals[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        let chat_status = app.project.workspaces[0].chats[0].status;
        app.mark_clean();

        app.set_terminal_restore_on_launch(terminal, false);
        app.mark_chat_status_by_id(chat, chat_status);

        assert!(!app.is_dirty());
    }

    #[test]
    fn structural_changes_ask_for_an_immediate_save_but_a_status_change_does_not() {
        let mut app = App::new(ProjectState::seeded());
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.mark_clean();

        assert!(app.mark_chat_status_by_id(chat, ChatStatus::Thinking));

        assert!(app.is_dirty());
        assert!(!app.needs_urgent_save());

        app.select_item(NavItem::Chat { workspace, chat });
        app.add_terminal_to_selected_workspace();

        assert!(app.needs_urgent_save());
        app.mark_clean();
        assert!(!app.needs_urgent_save());
    }

    #[test]
    fn a_config_reload_is_requested_once_and_taken_once() {
        let mut app = App::two_workspaces();
        assert!(!app.take_config_reload_request());

        app.request_config_reload();

        assert!(app.take_config_reload_request());
        assert!(!app.take_config_reload_request());
    }

    /// F5: a prompt is modal, so nothing is focused while one is open — and the
    /// focus it suspended comes back verbatim, rather than being re-derived from
    /// whatever happens to be selected when the prompt closes.
    #[test]
    fn a_prompt_suspends_the_browsing_focus_and_gives_it_back() {
        let mut app = App::two_workspaces();
        app.focus_sidebar();
        assert_eq!(app.active_focus(), Some(FocusMode::Sidebar));

        app.begin_command_palette();
        assert_eq!(app.active_focus(), None, "a prompt owns the keyboard");

        app.cancel_prompt();
        assert_eq!(app.active_focus(), Some(FocusMode::Sidebar));
    }
}
