use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{
    agent::{AgentEvent, AgentMessageRole, AgentTarget},
    config::ConfiguredProject,
    model::{
        AgentGeneration, AgentKind, ChatId, ChatMessage, ChatMessageRole, ChatStatus, ProjectState,
        PtyKey, TerminalId, TerminalStatus, WorkspaceId, DEFAULT_AGENT_CHAT_TITLE,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub project: ProjectState,
    /// The currently selected sidebar item by identity, or `None` when there are
    /// no nav items. Stored as an identity (not a bare index) so it can never be
    /// an out-of-range position; the invariant "valid item or None" is kept by
    /// `reconcile_selection` after every structural change.
    selected: Option<NavItem>,
    pub prompt: Option<Prompt>,
    pub focus: FocusMode,
    pub chat_buffers: BTreeMap<ChatId, ChatBuffer>,
    /// Chats whose current `Done` state the user has already seen — either by
    /// navigating onto them while finished, or because they finished while
    /// already selected. A `Done` chat in this set renders gray (inactive); a
    /// `Done` chat absent from it renders green (an unseen "finished"
    /// notification). Entries are dropped the moment a chat leaves `Done`, so
    /// re-prompting a finished agent arms a fresh notification. Runtime-only and
    /// keyed by the globally-unique `ChatId`, exactly like `chat_buffers`.
    seen_done: BTreeSet<ChatId>,
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

/// How long a transient notice stays on screen. Long enough to read a sentence
/// without looking for it, short enough that a burst of them during a daemon
/// outage does not permanently occupy rows.
pub const NOTICE_TTL: Duration = Duration::from_secs(12);

/// The most notices kept at once. Older ones are dropped rather than growing
/// the surface without bound.
pub const MAX_NOTICES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// Where a notice came from, so a condition that has ended can retract exactly
/// its own message without touching unrelated ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSource {
    /// The state file could not be written. Sticky: it describes a condition
    /// that is still true, and it is retracted by a successful save.
    SaveFailure,
    /// A workspace/chat/terminal mutation failed. Retracted by the next one
    /// that succeeds.
    Operation,
    /// Everything else — connection, protocol, config, state recovery.
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    level: NoticeLevel,
    source: NoticeSource,
    text: String,
    /// When the notice stops being rendered, or `None` for a notice describing
    /// a condition that is still true (only [`NoticeSource::SaveFailure`]).
    expires_at: Option<Instant>,
}

impl Notice {
    pub fn level(&self) -> NoticeLevel {
        self.level
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    OpenWorkspace(OpenWorkspacePrompt),
    NewTerminalCommand(TerminalCommandPrompt),
    CommandPalette(CommandPalettePrompt),
    Search(SearchPrompt),
    ConfirmDelete(DeleteConfirmationPrompt),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusMode {
    #[default]
    Sidebar,
    Chat,
    Terminal,
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
pub struct OpenWorkspacePrompt {
    pub input: PromptInput,
    pub error: Option<String>,
    pub selected: ListSelection,
    pub mode: OpenWorkspaceMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenWorkspaceMode {
    Path,
    ConfiguredProjects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWorkspaceMatch {
    pub name: String,
    pub path: PathBuf,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteConfirmationPrompt {
    target: DeleteTarget,
    pub description: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub scope: SearchScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionCell {
    pub row: i32,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub terminal: PtyKey,
    pub anchor: SelectionCell,
    pub focus: SelectionCell,
    pub dragging: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelectionRange {
    pub start: SelectionCell,
    pub end: SelectionCell,
}

impl TextSelection {
    pub fn normalized_range(self) -> TextSelectionRange {
        let anchor_key = (self.anchor.row, self.anchor.col);
        let focus_key = (self.focus.row, self.focus.col);
        if anchor_key <= focus_key {
            TextSelectionRange {
                start: self.anchor,
                end: self.focus,
            }
        } else {
            TextSelectionRange {
                start: self.focus,
                end: self.anchor,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Terminal(TerminalId),
    Chat(ChatId),
}

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
        Some("Ctrl+f"),
        "Open workspace",
        "import a workspace directory",
        Some(CommandAction::OpenWorkspace),
        BindingAvailability::Always,
    ),
    BindingEntry::global(
        Some("Ctrl+q"),
        "Delete selected item",
        "delete the selected chat/terminal or an empty workspace",
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
    BindingEntry::global(
        Some("Ctrl+n"),
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
    BindingEntry::reference(
        "Enter",
        "Submit, or confirm a deletion",
        BindingScope::Prompt,
    ),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteTarget {
    Workspace(WorkspaceId),
    Chat {
        workspace: WorkspaceId,
        chat: ChatId,
    },
    Terminal {
        workspace: WorkspaceId,
        terminal: TerminalId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatBuffer {
    lines: Vec<String>,
    partial: String,
    partial_role: Option<ChatMessageRole>,
}

/// One row of the sidebar, in render order. Only [`SidebarRow::Nav`] rows are
/// selectable; the others exist to be drawn and to occupy an index, which is
/// exactly what used to have to be re-derived by hand in `ui` (F14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarRow {
    /// The blank line separating two workspace groups.
    Spacer,
    /// A workspace's header line.
    Workspace(WorkspaceId),
    /// A selectable chat or terminal.
    Nav(NavItem),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavItem {
    Chat {
        workspace: WorkspaceId,
        chat: ChatId,
    },
    Terminal {
        workspace: WorkspaceId,
        terminal: TerminalId,
    },
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
        // A stopped command loaded from disk must require a deliberate start.
        // This also carries restoration safety across client restarts without
        // changing the durable schema: a vanished Running command is saved as
        // Stopped, then reconstructed here as recovery-required on next load.
        let recoverable_terminals = project
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter())
            .filter_map(|terminal| {
                (terminal.status == TerminalStatus::Stopped
                    && matches!(terminal.launch, crate::model::TerminalLaunch::Command(_)))
                .then_some(terminal.id)
            })
            .collect();
        let mut app = Self {
            project,
            selected: None,
            prompt: None,
            focus: FocusMode::Sidebar,
            chat_buffers,
            seen_done: BTreeSet::new(),
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

    pub fn record_save_failure(&mut self, message: impl Into<String>) {
        let message = message.into();
        // Sticky, because it describes a condition that is still true: state is
        // unsaved until a later save succeeds, and `mark_saved` retracts it.
        self.push_sticky_notice(
            NoticeLevel::Error,
            NoticeSource::SaveFailure,
            format!("State save failed: {message} — edit or quit to retry"),
        );
        self.save_error = Some(message);
    }

    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    fn record_operation_failure(&mut self, message: impl Into<String>) {
        self.clear_operation_error();
        self.push_notice(
            NoticeLevel::Error,
            NoticeSource::Operation,
            format!("Operation failed: {}", message.into()),
        );
    }

    fn clear_operation_error(&mut self) {
        self.notices
            .retain(|notice| notice.source != NoticeSource::Operation);
    }

    /// The transient status surface's current contents, oldest first.
    pub fn notices(&self) -> &[Notice] {
        &self.notices
    }

    /// Report something that has no pane to be reported into (E2).
    pub fn push_notice(
        &mut self,
        level: NoticeLevel,
        source: NoticeSource,
        text: impl Into<String>,
    ) -> bool {
        self.push_notice_at(Instant::now(), level, source, text)
    }

    /// [`Self::push_notice`] with the clock supplied, so tests are deterministic.
    pub fn push_notice_at(
        &mut self,
        now: Instant,
        level: NoticeLevel,
        source: NoticeSource,
        text: impl Into<String>,
    ) -> bool {
        self.insert_notice(Notice {
            level,
            source,
            text: text.into(),
            expires_at: Some(now + NOTICE_TTL),
        })
    }

    fn push_sticky_notice(
        &mut self,
        level: NoticeLevel,
        source: NoticeSource,
        text: impl Into<String>,
    ) -> bool {
        self.insert_notice(Notice {
            level,
            source,
            text: text.into(),
            expires_at: None,
        })
    }

    fn insert_notice(&mut self, notice: Notice) -> bool {
        // A failure that repeats every frame — a retrying reconnect, a save that
        // keeps failing — refreshes the row it already has instead of pushing a
        // fresh copy of the same sentence.
        if let Some(existing) = self
            .notices
            .iter_mut()
            .find(|existing| existing.text == notice.text && existing.level == notice.level)
        {
            existing.expires_at = notice.expires_at;
            existing.source = notice.source;
            // Nothing new is on screen, so this is not a reason to redraw.
            return false;
        }

        self.notices.push(notice);
        let overflow = self.notices.len().saturating_sub(MAX_NOTICES);
        self.notices.drain(..overflow);
        true
    }

    /// Drop notices whose time is up. Returns whether anything went away, so
    /// the render loop only rebuilds a frame when the surface actually changed.
    pub fn expire_notices(&mut self, now: Instant) -> bool {
        let before = self.notices.len();
        self.notices
            .retain(|notice| notice.expires_at.is_none_or(|deadline| deadline > now));
        self.notices.len() != before
    }

    /// Clear the surface on the user's request (`Ctrl+n` / the palette).
    pub fn dismiss_notices(&mut self) -> bool {
        let had_notices = !self.notices.is_empty();
        self.notices.clear();
        had_notices
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
        self.prompt.is_some()
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

    pub fn begin_command_palette(&mut self) {
        self.prompt = Some(Prompt::CommandPalette(CommandPalettePrompt {
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
        match &self.prompt {
            Some(Prompt::CommandPalette(prompt)) => {
                self.command_palette_entries_for(prompt.input.as_str())
            }
            _ => Vec::new(),
        }
    }

    pub fn open_workspace_matches(
        &self,
        projects: &[ConfiguredProject],
    ) -> Vec<OpenWorkspaceMatch> {
        if let Some(Prompt::OpenWorkspace(prompt)) = &self.prompt {
            if prompt.mode == OpenWorkspaceMode::ConfiguredProjects {
                return open_workspace_matches_for(prompt.input.as_str(), projects);
            }
        }

        Vec::new()
    }

    pub fn select_next_open_workspace_match(&mut self, projects: &[ConfiguredProject]) {
        self.move_open_workspace_selection(1, projects);
    }

    pub fn select_previous_open_workspace_match(&mut self, projects: &[ConfiguredProject]) {
        self.move_open_workspace_selection(-1, projects);
    }

    pub fn select_next_command_palette_entry(&mut self) {
        self.move_command_palette_selection(1);
    }

    pub fn select_previous_command_palette_entry(&mut self) {
        self.move_command_palette_selection(-1);
    }

    pub fn submit_command_palette(&mut self) -> Option<CommandAction> {
        let Some(Prompt::CommandPalette(prompt)) = &self.prompt else {
            return None;
        };
        let entries = self.command_palette_entries_for(prompt.input.as_str());
        let action = entries
            .get(prompt.selected.index().min(entries.len().saturating_sub(1)))
            .map(|entry| entry.action);
        self.prompt = None;
        action
    }

    pub fn begin_search(&mut self) -> bool {
        let Some(scope) = self.selected_search_scope() else {
            return false;
        };
        let input = self
            .active_search
            .as_ref()
            .filter(|search| search.scope == scope)
            .map(|search| search.query.clone())
            .unwrap_or_default();
        self.prompt = Some(Prompt::Search(SearchPrompt {
            input: PromptInput::new(input),
            scope,
            error: None,
        }));
        true
    }

    pub fn submit_search(&mut self) {
        let Some(Prompt::Search(prompt)) = &self.prompt else {
            return;
        };
        let query = prompt.input.as_str().trim().to_string();
        if query.is_empty() {
            self.active_search = None;
        } else {
            self.active_search = Some(SearchState {
                query,
                scope: prompt.scope,
            });
        }
        self.prompt = None;
    }

    pub fn clear_search(&mut self) {
        self.active_search = None;
    }

    pub fn begin_text_selection(&mut self, terminal: PtyKey, cell: SelectionCell) {
        self.text_selection = Some(TextSelection {
            terminal,
            anchor: cell,
            focus: cell,
            dragging: true,
        });
    }

    pub fn update_text_selection(&mut self, terminal: PtyKey, cell: SelectionCell) -> bool {
        let Some(selection) = &mut self.text_selection else {
            return false;
        };
        if selection.terminal != terminal {
            return false;
        }
        selection.focus = cell;
        true
    }

    pub fn end_text_selection(
        &mut self,
        terminal: PtyKey,
        cell: SelectionCell,
    ) -> Option<TextSelection> {
        if !self.update_text_selection(terminal, cell) {
            return None;
        }
        if let Some(selection) = &mut self.text_selection {
            selection.dragging = false;
            Some(*selection)
        } else {
            None
        }
    }

    pub fn clear_text_selection(&mut self) {
        self.text_selection = None;
    }

    pub fn shift_text_selection_rows(&mut self, terminal: PtyKey, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let Some(selection) = &mut self.text_selection else {
            return false;
        };
        if selection.terminal != terminal {
            return false;
        }

        let anchor_row = selection.anchor.row.saturating_add(delta);
        let focus_row = selection.focus.row.saturating_add(delta);
        if selection.anchor.row == anchor_row && selection.focus.row == focus_row {
            return false;
        }
        selection.anchor.row = anchor_row;
        selection.focus.row = focus_row;
        true
    }

    pub fn text_selection_for(&self, terminal: PtyKey) -> Option<&TextSelection> {
        self.text_selection
            .as_ref()
            .filter(|selection| selection.terminal == terminal)
    }

    /// Note (E12): the transcript this counts against is the structured one,
    /// which only the experimental process-agent backend writes and which
    /// nothing calls today, so the count is `0` for every chat a user can
    /// create. The chat pane says so where the user can see it; this helper is
    /// kept for the backend being wired up, not removed.
    pub fn chat_search_status(&self) -> Option<String> {
        let search = self.active_search.as_ref()?;
        let SearchScope::Chat(chat) = search.scope else {
            return None;
        };
        let count = filter_lines(self.chat_transcript_lines(chat), &search.query).len();
        Some(format!(
            "search chat: {count} match{} for `{}`",
            if count == 1 { "" } else { "es" },
            search.query
        ))
    }

    pub fn focus_sidebar(&mut self) {
        self.focus = FocusMode::Sidebar;
    }

    pub fn focus_selected_main(&mut self) -> bool {
        let Some(focus) = self.selected_main_focus() else {
            return false;
        };

        self.focus = focus;
        true
    }

    fn selected_main_focus(&self) -> Option<FocusMode> {
        match self.selected_item()? {
            NavItem::Chat { .. } => Some(FocusMode::Chat),
            NavItem::Terminal { .. } => Some(FocusMode::Terminal),
        }
    }

    fn sync_focus_to_selection(&mut self) {
        self.focus = self.selected_main_focus().unwrap_or(FocusMode::Sidebar);
        // Every selection change funnels through here (keyboard nav, mouse,
        // startup, post-delete reconcile), so it is the single place to record
        // that the user is now looking at the selected item: a finished agent
        // the user navigates onto stops being an unseen notification.
        self.mark_selected_done_seen();
    }

    /// Marks the currently selected chat's `Done` state as seen, if it is
    /// finished. A no-op for any other status or when a terminal is selected.
    fn mark_selected_done_seen(&mut self) {
        if let Some((workspace, chat)) = self.selected_chat_id() {
            if matches!(
                self.project.chat(workspace, chat).map(|chat| chat.status),
                Some(ChatStatus::Done)
            ) {
                self.seen_done.insert(chat);
            }
        }
    }

    /// Keeps `seen_done` consistent whenever a chat's status changes. A chat
    /// that finishes while the user is already looking at it counts as seen
    /// immediately (so it never flashes green); finishing in the background
    /// arms the green notification; leaving `Done` clears the flag so the next
    /// finish is notified afresh.
    fn reconcile_done_seen(&mut self, chat: ChatId, status: ChatStatus) {
        let seen_now = status == ChatStatus::Done
            && self.selected_chat_id().map(|(_, chat)| chat) == Some(chat);
        if seen_now {
            self.seen_done.insert(chat);
        } else {
            self.seen_done.remove(&chat);
        }
    }

    /// Whether the chat's current `Done` state has already been seen by the
    /// user. Only meaningful for finished chats; the renderer uses it to choose
    /// between the green "finished" notification and the gray inactive icon.
    pub fn chat_done_seen(&self, chat: ChatId) -> bool {
        self.seen_done.contains(&chat)
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

    fn move_open_workspace_selection(&mut self, delta: isize, projects: &[ConfiguredProject]) {
        let Some(Prompt::OpenWorkspace(prompt)) = &self.prompt else {
            return;
        };
        if prompt.mode != OpenWorkspaceMode::ConfiguredProjects {
            return;
        }
        let len = open_workspace_matches_for(prompt.input.as_str(), projects).len();
        if let Some(Prompt::OpenWorkspace(prompt)) = &mut self.prompt {
            prompt.selected.step(delta, len);
        }
    }

    fn move_command_palette_selection(&mut self, delta: isize) {
        let Some(Prompt::CommandPalette(prompt)) = &self.prompt else {
            return;
        };
        let len = self
            .command_palette_entries_for(prompt.input.as_str())
            .len();
        if let Some(Prompt::CommandPalette(prompt)) = &mut self.prompt {
            prompt.selected.step(delta, len);
        }
    }

    fn clamp_command_palette_selection(&mut self) {
        let Some(Prompt::CommandPalette(prompt)) = &self.prompt else {
            return;
        };
        let len = self
            .command_palette_entries_for(prompt.input.as_str())
            .len();
        if let Some(Prompt::CommandPalette(prompt)) = &mut self.prompt {
            prompt.selected.clamp(len);
        }
    }

    pub fn terminal_input_target(&self) -> Option<TerminalId> {
        self.selected_terminal_id().map(|(_, terminal)| terminal)
    }

    /// The PTY the selected nav item reads and writes: a chat's agent pane or
    /// a terminal's pane.
    pub fn pty_input_target(&self) -> Option<PtyKey> {
        match self.selected_item()? {
            NavItem::Chat { chat, .. } => Some(PtyKey::ChatAgent(chat)),
            NavItem::Terminal { terminal, .. } => Some(PtyKey::Terminal(terminal)),
        }
    }

    /// The single source of truth for the sidebar: a blank row between
    /// workspaces, then each workspace's header, its chats and its terminals.
    ///
    /// Both the render order and the navigation order come from this one walk
    /// (F14). `ui` renders the rows it yields and finds the highlight by
    /// position in them; every nav query
    /// (`nav_items`/`nav_len`/`nav_item_at`/`nav_item_position`) is the same
    /// walk with the non-selectable rows filtered out. Nothing re-derives
    /// either order by hand, so a change here cannot move the highlight off
    /// the row it belongs to without a compile error.
    fn sidebar_row_iter(&self) -> impl Iterator<Item = SidebarRow> + '_ {
        self.project
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(index, workspace)| {
                let spacer = (index > 0).then_some(SidebarRow::Spacer);
                let header = std::iter::once(SidebarRow::Workspace(workspace.id));
                let chats = workspace.chats.iter().map(move |chat| {
                    SidebarRow::Nav(NavItem::Chat {
                        workspace: workspace.id,
                        chat: chat.id,
                    })
                });
                let terminals = workspace.terminals.iter().map(move |terminal| {
                    SidebarRow::Nav(NavItem::Terminal {
                        workspace: workspace.id,
                        terminal: terminal.id,
                    })
                });
                spacer
                    .into_iter()
                    .chain(header)
                    .chain(chats)
                    .chain(terminals)
            })
    }

    /// The sidebar's rows in render order. Collected rather than lazy because
    /// the renderer needs both the rows and the highlight's index into them.
    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        self.sidebar_row_iter().collect()
    }

    /// The selectable subset of [`Self::sidebar_row_iter`], in the same order:
    /// each workspace's chats followed by its terminals, across all workspaces.
    fn nav_iter(&self) -> impl Iterator<Item = NavItem> + '_ {
        self.sidebar_row_iter().filter_map(|row| match row {
            SidebarRow::Nav(item) => Some(item),
            SidebarRow::Spacer | SidebarRow::Workspace(_) => None,
        })
    }

    pub fn nav_items(&self) -> Vec<NavItem> {
        self.nav_iter().collect()
    }

    pub fn nav_len(&self) -> usize {
        self.nav_iter().count()
    }

    pub fn nav_item_at(&self, target_index: usize) -> Option<NavItem> {
        self.nav_iter().nth(target_index)
    }

    pub fn selected_item(&self) -> Option<NavItem> {
        // Validate against the current tree so a selection left stale by a direct
        // project mutation is never observable (it resolves to `None`, exactly as
        // the old index-into-list lookup did when out of range).
        self.selected
            .filter(|item| self.nav_item_position(*item).is_some())
    }

    /// The position of the current selection in the nav list, if any. Used for
    /// rendering (the sidebar highlight) and by `select_next`/`select_previous`.
    pub fn selected_index(&self) -> Option<usize> {
        self.selected.and_then(|item| self.nav_item_position(item))
    }

    /// Select the nav item at `index`, if one exists there (otherwise a no-op).
    pub fn select_nav_index(&mut self, index: usize) {
        if let Some(item) = self.nav_item_at(index) {
            self.select_item(item);
        }
    }

    pub fn selected_workspace_id(&self) -> Option<WorkspaceId> {
        match self.selected_item() {
            Some(NavItem::Chat { workspace, .. }) | Some(NavItem::Terminal { workspace, .. }) => {
                Some(workspace)
            }
            None => self
                .project
                .workspaces
                .first()
                .map(|workspace| workspace.id),
        }
    }

    pub fn selected_terminal_id(&self) -> Option<(WorkspaceId, TerminalId)> {
        match self.selected_item() {
            Some(NavItem::Terminal {
                workspace,
                terminal,
            }) => Some((workspace, terminal)),
            _ => None,
        }
    }

    pub fn selected_chat_id(&self) -> Option<(WorkspaceId, ChatId)> {
        match self.selected_item() {
            Some(NavItem::Chat { workspace, chat }) => Some((workspace, chat)),
            _ => None,
        }
    }

    pub fn selected_search_scope(&self) -> Option<SearchScope> {
        match self.selected_item()? {
            NavItem::Chat { chat, .. } => Some(SearchScope::Chat(chat)),
            NavItem::Terminal { terminal, .. } => Some(SearchScope::Terminal(terminal)),
        }
    }

    pub fn selected_item_can_be_deleted(&self) -> bool {
        self.selected_delete_target().is_some()
    }

    pub fn selected_item_can_start_input(&self) -> bool {
        self.selected_chat_id().is_some() || self.selected_terminal_id().is_some()
    }

    pub fn selected_item_can_search(&self) -> bool {
        self.selected_search_scope().is_some()
    }

    pub fn begin_delete_selected(&mut self) -> bool {
        let Some(target) = self.selected_delete_target() else {
            return false;
        };
        let description = self.delete_target_description(target);
        self.prompt = Some(Prompt::ConfirmDelete(DeleteConfirmationPrompt {
            target,
            description,
            error: None,
        }));
        true
    }

    pub fn pending_delete_target(&self) -> Option<DeleteTarget> {
        match &self.prompt {
            Some(Prompt::ConfirmDelete(prompt)) => Some(prompt.target),
            _ => None,
        }
    }

    pub fn pending_delete_pty(&self) -> Option<PtyKey> {
        match self.pending_delete_target()? {
            DeleteTarget::Workspace(_) => None,
            DeleteTarget::Chat { chat, .. } => Some(PtyKey::ChatAgent(chat)),
            DeleteTarget::Terminal { terminal, .. } => Some(PtyKey::Terminal(terminal)),
        }
    }

    pub fn set_delete_error(&mut self, message: impl Into<String>) {
        if let Some(Prompt::ConfirmDelete(prompt)) = &mut self.prompt {
            prompt.error = Some(message.into());
        }
    }

    pub fn confirm_delete(&mut self) -> Vec<PtyKey> {
        let Some(Prompt::ConfirmDelete(prompt)) = self.prompt.take() else {
            return Vec::new();
        };
        self.delete_target(prompt.target)
    }

    fn delete_target(&mut self, target: DeleteTarget) -> Vec<PtyKey> {
        // Remember where the selection sat so that, if the selected item is the
        // one being removed, the selection lands on whatever shifts into its slot.
        let previous_index = self.selected_index();
        let mut runtime_terminals = Vec::new();
        match target {
            DeleteTarget::Workspace(workspace_id) => {
                let can_remove = self
                    .project
                    .workspace(workspace_id)
                    .is_some_and(|workspace| {
                        workspace.chats.is_empty() && workspace.terminals.is_empty()
                    });
                if can_remove && self.project.remove_workspace(workspace_id).is_some() {
                    self.workspace_git_branches.remove(&workspace_id);
                    self.mark_structural_change();
                }
            }
            DeleteTarget::Chat { workspace, chat } => {
                if self.project.remove_chat(workspace, chat).is_some() {
                    runtime_terminals.push(PtyKey::ChatAgent(chat));
                    self.chat_buffers.remove(&chat);
                    self.seen_done.remove(&chat);
                    self.mark_structural_change();
                    self.remove_workspace_if_empty(workspace);
                }
            }
            DeleteTarget::Terminal {
                workspace,
                terminal,
            } => {
                if self.project.remove_terminal(workspace, terminal).is_some() {
                    runtime_terminals.push(PtyKey::Terminal(terminal));
                    self.recoverable_terminals.remove(&terminal);
                    self.mark_structural_change();
                    self.remove_workspace_if_empty(workspace);
                }
            }
        }

        self.reconcile_selection(previous_index);
        self.sync_focus_to_selection();
        runtime_terminals
    }

    fn delete_target_description(&self, target: DeleteTarget) -> String {
        match target {
            DeleteTarget::Workspace(workspace) => self
                .project
                .workspace(workspace)
                .map(|workspace| format!("empty workspace `{}`", workspace.name))
                .unwrap_or_else(|| "missing workspace".to_string()),
            DeleteTarget::Chat { workspace, chat } => self
                .project
                .chat(workspace, chat)
                .map(|chat| format!("{} chat `{}`", chat.agent.display_name(), chat.name))
                .unwrap_or_else(|| "missing chat".to_string()),
            DeleteTarget::Terminal {
                workspace,
                terminal,
            } => self
                .project
                .terminal(workspace, terminal)
                .map(|terminal| format!("terminal `{}`", terminal.name))
                .unwrap_or_else(|| "missing terminal".to_string()),
        }
    }

    fn selected_delete_target(&self) -> Option<DeleteTarget> {
        match self.selected_item() {
            Some(NavItem::Chat { workspace, chat }) => Some(DeleteTarget::Chat { workspace, chat }),
            Some(NavItem::Terminal {
                workspace,
                terminal,
            }) => Some(DeleteTarget::Terminal {
                workspace,
                terminal,
            }),
            None => self.first_empty_workspace_id().map(DeleteTarget::Workspace),
        }
    }

    fn first_empty_workspace_id(&self) -> Option<WorkspaceId> {
        self.project
            .workspaces
            .iter()
            .find(|workspace| workspace.chats.is_empty() && workspace.terminals.is_empty())
            .map(|workspace| workspace.id)
    }

    fn remove_workspace_if_empty(&mut self, workspace_id: WorkspaceId) {
        let is_empty = self
            .project
            .workspace(workspace_id)
            .is_some_and(|workspace| workspace.chats.is_empty() && workspace.terminals.is_empty());
        if !is_empty {
            return;
        }

        if self.project.remove_workspace(workspace_id).is_some() {
            self.workspace_git_branches.remove(&workspace_id);
            self.mark_structural_change();
        }
    }

    pub fn mark_terminal_running(&mut self, terminal: TerminalId) {
        self.recoverable_terminals.remove(&terminal);
        if let Some(terminal) = self.project.terminal_mut_by_id(terminal) {
            if terminal.status != TerminalStatus::Running {
                terminal.status = TerminalStatus::Running;
                self.dirty = true;
            }
        }
    }

    pub fn mark_terminal_recoverable(&mut self, terminal: TerminalId) {
        if self
            .project
            .terminal_mut_by_id(terminal)
            .is_some_and(|terminal| {
                matches!(terminal.launch, crate::model::TerminalLaunch::Command(_))
            })
        {
            self.recoverable_terminals.insert(terminal);
        }
    }

    pub fn terminal_requires_recovery(&self, terminal: TerminalId) -> bool {
        self.recoverable_terminals.contains(&terminal)
    }

    pub fn mark_terminal_stopped(&mut self, terminal: TerminalId) {
        if let Some(terminal) = self.project.terminal_mut_by_id(terminal) {
            if terminal.status != TerminalStatus::Stopped {
                terminal.status = TerminalStatus::Stopped;
                self.dirty = true;
            }
        }
    }

    /// `lines` is a closure rather than a value because the caller's only source
    /// of terminal text is a full screen scrape (a `String` per row): with no
    /// search active — the common case, every frame — this returns before the
    /// scrape ever runs.
    pub fn terminal_search_matches(
        &self,
        terminal: TerminalId,
        lines: impl FnOnce() -> Vec<String>,
    ) -> Option<Vec<String>> {
        let search = self.active_search.as_ref()?;
        if search.scope != SearchScope::Terminal(terminal) {
            return None;
        }
        Some(filter_lines(lines(), &search.query))
    }

    pub fn terminal_search_status(
        &self,
        terminal: TerminalId,
        lines: Vec<String>,
    ) -> Option<String> {
        let search = self.active_search.as_ref()?;
        if search.scope != SearchScope::Terminal(terminal) {
            return None;
        }
        let count = filter_lines(lines, &search.query).len();
        Some(format!(
            "search terminal: {count} match{} for `{}`",
            if count == 1 { "" } else { "es" },
            search.query
        ))
    }

    pub fn chat_lines(&self, chat: ChatId) -> Vec<String> {
        self.chat_buffers
            .get(&chat)
            .map(ChatBuffer::visible_lines)
            .unwrap_or_default()
    }

    pub fn chat_transcript_lines(&self, chat: ChatId) -> Vec<String> {
        let mut lines = self
            .project
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter())
            .find(|session| session.id == chat)
            .map(|session| transcript_lines(&session.messages))
            .unwrap_or_default();
        if lines.is_empty() {
            lines = self.chat_lines(chat);
        }
        lines
    }

    pub fn filtered_chat_lines(&self, chat: ChatId) -> Option<Vec<String>> {
        let search = self.active_search.as_ref()?;
        if search.scope != SearchScope::Chat(chat) {
            return None;
        }
        Some(filter_lines(
            self.chat_transcript_lines(chat),
            &search.query,
        ))
    }

    pub fn active_search_query_for_chat(&self, chat: ChatId) -> Option<&str> {
        self.active_search
            .as_ref()
            .filter(|search| search.scope == SearchScope::Chat(chat))
            .map(|search| search.query.as_str())
    }

    fn append_chat_message(
        &mut self,
        target: AgentTarget,
        role: ChatMessageRole,
        text: impl Into<String>,
    ) {
        let text = text.into();
        self.chat_buffers
            .entry(target.chat)
            .or_default()
            .append_delta(role, &format!("{text}\n"));
        if self
            .project
            .append_chat_message(target.workspace, target.chat, role, text)
        {
            self.dirty = true;
        }
    }

    pub fn apply_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::MessageDelta {
                target, role, text, ..
            } => {
                let role = chat_role_from_agent(role);
                self.chat_buffers
                    .entry(target.chat)
                    .or_default()
                    .append_delta(role, &text);
                if self
                    .project
                    .append_chat_delta(target.workspace, target.chat, role, &text)
                {
                    self.dirty = true;
                }
            }
            AgentEvent::ToolCall {
                target,
                name,
                arguments,
            } => {
                let text = if arguments.is_empty() {
                    name
                } else {
                    format!("{name} {arguments}")
                };
                self.append_chat_message(target, ChatMessageRole::Tool, text);
            }
            AgentEvent::FileChanged { target, path } => {
                self.append_chat_message(
                    target,
                    ChatMessageRole::System,
                    format!("file changed: {}", path.display()),
                );
            }
            AgentEvent::CommandStarted { target, command } => {
                self.append_chat_message(
                    target,
                    ChatMessageRole::System,
                    format!("cmd: {command}"),
                );
            }
            AgentEvent::StatusChanged { target, status } => {
                let changed = self
                    .project
                    .chat_mut(target.workspace, target.chat)
                    .is_some_and(|chat| {
                        let changed = chat.status != status;
                        chat.status = status;
                        changed
                    });
                if changed {
                    self.dirty = true;
                    self.reconcile_done_seen(target.chat, status);
                }
            }
            AgentEvent::Error { target, message } => {
                let changed = self
                    .project
                    .chat_mut(target.workspace, target.chat)
                    .is_some_and(|chat| {
                        let changed = chat.status != ChatStatus::Failed;
                        chat.status = ChatStatus::Failed;
                        changed
                    });
                if changed {
                    self.dirty = true;
                    self.reconcile_done_seen(target.chat, ChatStatus::Failed);
                }
                self.append_chat_message(target, ChatMessageRole::Error, message);
            }
        }
    }

    pub fn select_next(&mut self) {
        let len = self.nav_len();
        if len > 0 {
            let next = self.selected_index().map_or(0, |index| (index + 1) % len);
            self.selected = self.nav_item_at(next);
            self.sync_focus_to_selection();
        }
    }

    pub fn select_previous(&mut self) {
        let len = self.nav_len();
        if len > 0 {
            let previous = self
                .selected_index()
                .map_or(len - 1, |index| index.checked_sub(1).unwrap_or(len - 1));
            self.selected = self.nav_item_at(previous);
            self.sync_focus_to_selection();
        }
    }

    pub fn begin_open_workspace(&mut self, projects: &[ConfiguredProject]) {
        let has_configured_projects = !projects.is_empty();
        let input = if has_configured_projects {
            String::new()
        } else {
            std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        };

        self.prompt = Some(Prompt::OpenWorkspace(OpenWorkspacePrompt {
            input: PromptInput::new(input),
            error: None,
            selected: ListSelection::default(),
            mode: if has_configured_projects {
                OpenWorkspaceMode::ConfiguredProjects
            } else {
                OpenWorkspaceMode::Path
            },
        }));
    }

    pub fn begin_new_terminal_command(&mut self) -> bool {
        if self.selected_workspace_id().is_none() {
            return false;
        }

        self.prompt = Some(Prompt::NewTerminalCommand(TerminalCommandPrompt {
            input: PromptInput::default(),
            error: None,
        }));
        true
    }

    pub fn cancel_prompt(&mut self) {
        self.prompt = None;
    }

    pub fn begin_terminal_input(&mut self) -> bool {
        if self.selected_terminal_id().is_none() {
            return false;
        }

        self.focus = FocusMode::Terminal;
        true
    }

    pub fn begin_chat_agent_input(&mut self) -> bool {
        if self.selected_chat_id().is_none() {
            return false;
        }

        self.focus = FocusMode::Chat;
        true
    }

    pub fn end_pty_input(&mut self) {
        self.sync_focus_to_selection();
    }

    pub fn begin_agent_generation(
        &mut self,
        chat: ChatId,
    ) -> Result<Option<AgentGeneration>, crate::model::IdAllocationError> {
        let generation = self.project.begin_agent_generation(chat)?;
        if generation.is_some() {
            self.dirty = true;
        }
        Ok(generation)
    }

    pub fn clear_agent_generation(&mut self, chat: ChatId, generation: AgentGeneration) -> bool {
        let changed = self.project.clear_agent_generation(chat, generation);
        self.dirty |= changed;
        changed
    }

    /// Returns whether the chat's status actually changed (useful for deciding
    /// if a redraw is needed).
    pub fn mark_chat_status_by_id(&mut self, chat: ChatId, status: ChatStatus) -> bool {
        let mut changed = false;
        for chat_session in self
            .project
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.chats.iter_mut())
        {
            if chat_session.id == chat {
                if chat_session.status != status {
                    chat_session.status = status;
                    self.dirty = true;
                    changed = true;
                }
                break;
            }
        }
        if changed {
            self.reconcile_done_seen(chat, status);
        }
        changed
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
        let mutated = match &mut self.prompt {
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
        match &self.prompt {
            Some(Prompt::OpenWorkspace(prompt)) => Some(&prompt.input),
            Some(Prompt::NewTerminalCommand(prompt)) => Some(&prompt.input),
            Some(Prompt::CommandPalette(prompt)) => Some(&prompt.input),
            Some(Prompt::Search(prompt)) => Some(&prompt.input),
            Some(Prompt::ConfirmDelete(_)) | None => None,
        }
    }

    pub fn submit_open_workspace(&mut self, projects: &[ConfiguredProject]) {
        let Some(Prompt::OpenWorkspace(prompt)) = &self.prompt else {
            return;
        };
        let raw_input = prompt.input.as_str().trim().to_string();
        let selected = prompt.selected.index();
        let mode = prompt.mode;

        if mode == OpenWorkspaceMode::ConfiguredProjects {
            let matches = open_workspace_matches_for(&raw_input, projects);
            if let Some(project) = matches.get(selected.min(matches.len().saturating_sub(1))) {
                self.import_workspace_path(expand_path(&project.path), Some(project.name.clone()));
                return;
            }

            if raw_input.is_empty() {
                self.set_open_workspace_error("select a configured project");
                return;
            }
            if !looks_like_path(&raw_input) {
                self.set_open_workspace_error("no matching configured project");
                return;
            }
        } else if raw_input.is_empty() {
            self.set_open_workspace_error("enter a directory path");
            return;
        }

        self.import_workspace_path(expand_tilde(&raw_input), None);
    }

    fn import_workspace_path(&mut self, path: PathBuf, configured_name: Option<String>) {
        let Ok(cwd) = std::fs::canonicalize(&path) else {
            self.set_open_workspace_error("path does not exist");
            return;
        };

        if !cwd.is_dir() {
            self.set_open_workspace_error("path is not a directory");
            return;
        }

        if let Some(existing_workspace) = self
            .project
            .workspaces
            .iter()
            .find(|workspace| workspace.cwd.as_deref() == Some(cwd.as_path()))
        {
            let workspace = existing_workspace.id;
            self.prompt = None;
            self.select_first_item_in_workspace(workspace);
            return;
        }

        let name = configured_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| workspace_name(&cwd));
        // Stage both allocations so terminal-ID exhaustion cannot leave a
        // half-imported workspace in memory.
        let mut project = self.project.clone();
        let workspace = match project.add_workspace(name, Some(cwd)) {
            Ok(workspace) => workspace,
            Err(error) => {
                self.set_open_workspace_error(error.to_string());
                return;
            }
        };
        match project.add_terminal(workspace, "shell".to_string(), TerminalStatus::Stopped) {
            Ok(Some(_)) => {}
            Ok(None) => {
                self.set_open_workspace_error("new workspace disappeared during import");
                return;
            }
            Err(error) => {
                self.set_open_workspace_error(error.to_string());
                return;
            }
        }
        self.project = project;

        self.prompt = None;
        self.select_first_item_in_workspace(workspace);
        self.clear_operation_error();
        self.mark_structural_change();
    }

    pub fn submit_new_terminal_command(&mut self) {
        let Some(Prompt::NewTerminalCommand(prompt)) = &self.prompt else {
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

        match self.project.add_command_terminal(
            workspace,
            name.clone(),
            TerminalStatus::Stopped,
            command,
        ) {
            Ok(Some(terminal)) => {
                self.prompt = None;
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

    pub fn add_chat_to_selected_workspace_and_return(
        &mut self,
        agent: AgentKind,
    ) -> Option<(WorkspaceId, ChatId)> {
        let workspace = self.selected_workspace_id()?;
        let name = DEFAULT_AGENT_CHAT_TITLE.to_string();
        let chat = match self
            .project
            .add_chat(workspace, name, ChatStatus::Idle, agent)
        {
            Ok(Some(chat)) => chat,
            Ok(None) => {
                self.record_operation_failure("selected workspace no longer exists");
                return None;
            }
            Err(error) => {
                self.record_operation_failure(error.to_string());
                return None;
            }
        };
        self.select_item(NavItem::Chat { workspace, chat });
        self.clear_operation_error();
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
        match self
            .project
            .add_terminal(workspace, name.clone(), TerminalStatus::Stopped)
        {
            Ok(Some(terminal)) => {
                self.select_item(NavItem::Terminal {
                    workspace,
                    terminal,
                });
                self.clear_operation_error();
                self.mark_structural_change();
            }
            Ok(None) => self.record_operation_failure("selected workspace no longer exists"),
            Err(error) => self.record_operation_failure(error.to_string()),
        }
    }

    fn set_open_workspace_error(&mut self, message: impl Into<String>) {
        if let Some(Prompt::OpenWorkspace(prompt)) = &mut self.prompt {
            prompt.error = Some(message.into());
        }
    }

    fn set_terminal_command_error(&mut self, message: impl Into<String>) {
        if let Some(Prompt::NewTerminalCommand(prompt)) = &mut self.prompt {
            prompt.error = Some(message.into());
        }
    }

    pub fn select_item(&mut self, target: NavItem) {
        if self.nav_item_position(target).is_some() {
            self.selected = Some(target);
        } else {
            self.reconcile_selection(self.selected_index());
        }
        self.sync_focus_to_selection();
    }

    fn select_first_item_in_workspace(&mut self, workspace_id: WorkspaceId) -> bool {
        let Some(workspace) = self.project.workspace(workspace_id) else {
            self.reconcile_selection(self.selected_index());
            self.sync_focus_to_selection();
            return false;
        };

        let target = workspace
            .chats
            .first()
            .map(|chat| NavItem::Chat {
                workspace: workspace_id,
                chat: chat.id,
            })
            .or_else(|| {
                workspace
                    .terminals
                    .first()
                    .map(|terminal| NavItem::Terminal {
                        workspace: workspace_id,
                        terminal: terminal.id,
                    })
            });

        if let Some(target) = target {
            self.select_item(target);
            true
        } else {
            self.reconcile_selection(self.selected_index());
            self.sync_focus_to_selection();
            false
        }
    }

    fn nav_item_position(&self, target: NavItem) -> Option<usize> {
        self.nav_iter().position(|item| item == target)
    }

    /// Re-establish the selection invariant after a structural change: keep the
    /// current selection if it still exists, otherwise select the item now at
    /// `preferred_index` (clamped to the list), or `None` when the list is empty.
    /// This replaces the old index clamp and preserves position-stable selection
    /// — e.g. deleting the selected item selects whatever shifts into its slot.
    fn reconcile_selection(&mut self, preferred_index: Option<usize>) {
        let len = self.nav_len();
        if len == 0 {
            self.selected = None;
            return;
        }
        if let Some(selected) = self.selected {
            if self.nav_item_position(selected).is_some() {
                return;
            }
        }
        let index = preferred_index.unwrap_or(0).min(len - 1);
        self.selected = self.nav_item_at(index);
    }
}

fn open_workspace_matches_for(
    query: &str,
    projects: &[ConfiguredProject],
) -> Vec<OpenWorkspaceMatch> {
    let query = query.trim();
    let mut matches = projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| {
            fuzzy_project_score(&project.name, query).map(|score| {
                (
                    score,
                    index,
                    OpenWorkspaceMatch {
                        name: project.name.clone(),
                        path: project.path.clone(),
                    },
                )
            })
        })
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    matches.into_iter().map(|(_, _, project)| project).collect()
}

fn fuzzy_project_score(name: &str, query: &str) -> Option<i64> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }

    let name_lower = name.to_lowercase();
    query.split_whitespace().try_fold(0, |score, term| {
        fuzzy_term_score(&name_lower, term).map(|term_score| score + term_score)
    })
}

fn fuzzy_term_score(name: &str, term: &str) -> Option<i64> {
    if term.is_empty() {
        return Some(0);
    }

    let name_chars = name.chars().collect::<Vec<_>>();
    let term_chars = term.chars().collect::<Vec<_>>();
    let mut score = if name.contains(term) { 20 } else { 0 };
    let mut position = 0;
    let mut last_match: Option<usize> = None;

    for ch in term_chars {
        while position < name_chars.len() && name_chars[position] != ch {
            position += 1;
        }
        if position == name_chars.len() {
            return None;
        }

        score += 10;
        if position == 0 {
            score += 8;
        } else if is_name_boundary(name_chars[position.saturating_sub(1)]) {
            score += 6;
        }
        if let Some(previous) = last_match {
            if position == previous + 1 {
                score += 5;
            } else {
                score -= (position - previous - 1).min(8) as i64;
            }
        }

        last_match = Some(position);
        position += 1;
    }

    score -= name_chars.len().saturating_sub(term.len()).min(16) as i64;
    Some(score)
}

fn is_name_boundary(ch: char) -> bool {
    matches!(ch, '-' | '_' | ' ' | '/' | '.')
}

fn transcript_lines(messages: &[ChatMessage]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| {
            let prefix = format!("{} > ", message.role.label());
            message
                .text
                .lines()
                .map(move |line| format!("{prefix}{line}"))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn filter_lines(lines: Vec<String>, query: &str) -> Vec<String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return lines;
    }

    lines
        .into_iter()
        .filter(|line| line.to_lowercase().contains(&query))
        .collect()
}

impl ChatBuffer {
    const MAX_LINES: usize = 500;

    fn from_messages(messages: &[ChatMessage]) -> Self {
        let mut buffer = Self::default();
        for message in messages {
            buffer.append_delta(message.role, &message.text);
            buffer.flush_partial();
        }
        buffer
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty() && !self.partial_has_content()
    }

    fn append_delta(&mut self, role: ChatMessageRole, text: &str) {
        if self.partial_role != Some(role) {
            self.flush_partial();
            self.partial = format!("{} > ", role.label());
            self.partial_role = Some(role);
        }

        for ch in text.chars() {
            match ch {
                '\n' => {
                    self.flush_partial();
                    self.partial = format!("{} > ", role.label());
                    self.partial_role = Some(role);
                }
                '\r' => {
                    self.partial = format!("{} > ", role.label());
                    self.partial_role = Some(role);
                }
                '\t' => self.partial.push(' '),
                ch if ch.is_control() => {}
                ch => self.partial.push(ch),
            }
        }
    }

    fn visible_lines(&self) -> Vec<String> {
        let mut lines = self.lines.clone();
        if self.partial_has_content() {
            lines.push(self.partial.clone());
        }
        lines
    }

    fn flush_partial(&mut self) {
        if self.partial_has_content() {
            let line = std::mem::take(&mut self.partial);
            self.push_line(line);
        } else {
            self.partial.clear();
        }
        self.partial_role = None;
    }

    fn partial_has_content(&self) -> bool {
        self.partial_role
            .is_some_and(|role| self.partial != format!("{} > ", role.label()))
    }

    fn push_line(&mut self, line: String) {
        self.lines.push(line);
        let overflow = self.lines.len().saturating_sub(Self::MAX_LINES);
        if overflow > 0 {
            self.lines.drain(..overflow);
        }
    }
}

fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return std::env::var_os("HOME").map_or_else(|| PathBuf::from(input), PathBuf::from);
    }

    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    PathBuf::from(input)
}

fn expand_path(path: &Path) -> PathBuf {
    path.to_str()
        .map(expand_tilde)
        .unwrap_or_else(|| path.to_path_buf())
}

fn looks_like_path(input: &str) -> bool {
    let input = input.trim();
    Path::new(input).is_absolute()
        || input.starts_with('~')
        || input.starts_with('.')
        || input.contains(std::path::MAIN_SEPARATOR)
}

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
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

fn command_terminal_name(command: &str, next: usize) -> String {
    let command = command.trim();
    if command.is_empty() {
        format!("command-{next}")
    } else {
        command.to_string()
    }
}

fn chat_role_from_agent(role: AgentMessageRole) -> ChatMessageRole {
    match role {
        AgentMessageRole::User => ChatMessageRole::User,
        AgentMessageRole::Assistant => ChatMessageRole::Assistant,
        AgentMessageRole::System => ChatMessageRole::System,
        AgentMessageRole::Tool => ChatMessageRole::Tool,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

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
    fn new_chats_use_agent_title() {
        let mut app = App::default();
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
        let mut app = App::default();
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

    #[test]
    fn text_selection_rows_shift_with_viewport_scroll() {
        let mut app = App::default();
        let terminal = PtyKey::Terminal(TerminalId(9));
        app.begin_text_selection(terminal, SelectionCell { row: 1, col: 0 });
        app.update_text_selection(terminal, SelectionCell { row: 1, col: 2 });

        assert!(app.shift_text_selection_rows(terminal, 3));
        let selection = app.text_selection_for(terminal).expect("selection remains");
        assert_eq!(selection.anchor.row, 4);
        assert_eq!(selection.focus.row, 4);

        assert!(app.shift_text_selection_rows(terminal, -5));
        let selection = app.text_selection_for(terminal).expect("selection remains");
        assert_eq!(selection.anchor.row, -1);
        assert_eq!(selection.focus.row, -1);
    }

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
        assert_eq!(app.focus, FocusMode::Terminal);
        assert!(app.is_dirty());
    }

    #[test]
    fn persisted_stopped_command_requires_deliberate_recovery_until_running() {
        let mut state = ProjectState::try_first_run().expect("first-run project");
        let terminal = state.workspaces[0].terminals[0].id;
        state.workspaces[0].terminals[0].launch =
            crate::model::TerminalLaunch::Command("cargo test".to_string());
        state.workspaces[0].terminals[0].status = TerminalStatus::Stopped;

        let mut app = App::new(state);
        assert!(app.terminal_requires_recovery(terminal));

        app.mark_terminal_running(terminal);
        assert!(!app.terminal_requires_recovery(terminal));
    }

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
        assert_eq!(app.prompt, None);
    }

    #[test]
    fn terminal_search_filters_all_scrollback_lines() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        assert!(app.begin_search());
        for ch in "alp".chars() {
            app.push_prompt_char(ch);
        }
        app.submit_search();

        let lines = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        assert_eq!(
            app.terminal_search_matches(terminal, || lines.clone()),
            Some(vec!["alpha".to_string()])
        );
        assert!(app
            .terminal_search_status(terminal, lines)
            .unwrap()
            .contains("1 match"));
    }

    #[test]
    fn terminal_search_matches_skips_the_scrape_without_an_active_search() {
        let app = App::default();
        let terminal = app.project.workspaces[0].terminals[0].id;
        let mut scraped = false;

        let matches = app.terminal_search_matches(terminal, || {
            scraped = true;
            Vec::new()
        });

        assert_eq!(matches, None);
        assert!(
            !scraped,
            "with no search active the caller's screen scrape must never run"
        );
    }

    #[test]
    fn chat_search_filters_persisted_transcript_lines() {
        let mut state = ProjectState::seeded();
        let workspace = state.workspaces[0].id;
        let chat = state.workspaces[0].chats[0].id;
        state.append_chat_message(
            workspace,
            chat,
            ChatMessageRole::Assistant,
            "first\nneedle here".to_string(),
        );
        let mut app = App::new(state);
        app.select_item(NavItem::Chat { workspace, chat });

        assert!(app.begin_search());
        for ch in "needle".chars() {
            app.push_prompt_char(ch);
        }
        app.submit_search();

        assert_eq!(
            app.filtered_chat_lines(chat),
            Some(vec!["agent > needle here".to_string()])
        );
    }

    #[test]
    fn delete_selected_terminal_requires_confirmation() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });

        assert!(app.begin_delete_selected());
        assert_eq!(app.pending_delete_pty(), Some(PtyKey::Terminal(terminal)));
        assert!(app.project.terminal(workspace, terminal).is_some());

        let runtime_terminals = app.confirm_delete();
        assert_eq!(runtime_terminals, vec![PtyKey::Terminal(terminal)]);
        assert!(app.project.terminal(workspace, terminal).is_none());
        assert!(app.is_dirty());
    }

    #[test]
    fn delete_confirmation_can_be_cancelled_without_mutation() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        app.mark_clean();

        assert!(app.begin_delete_selected());
        app.cancel_prompt();

        assert!(app.project.terminal(workspace, terminal).is_some());
        assert_eq!(app.prompt, None);
        assert!(!app.is_dirty());
    }

    #[test]
    fn delete_selected_chat_removes_transcript_and_pi_runtime_id() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });
        app.chat_buffers.insert(chat, ChatBuffer::default());
        let pi_terminal = PtyKey::ChatAgent(chat);
        assert!(app.begin_delete_selected());
        let runtime_terminals = app.confirm_delete();

        assert_eq!(runtime_terminals, vec![pi_terminal]);
        assert!(app.project.chat(workspace, chat).is_none());
        assert!(!app.chat_buffers.contains_key(&chat));
        assert!(app.is_dirty());
    }

    #[test]
    fn workspaces_are_not_selectable_nav_items() {
        let app = App::seeded();
        let first_workspace = app.project.workspaces[0].id;
        let first_chat = app.project.workspaces[0].chats[0].id;

        assert_eq!(app.nav_len(), 5);
        assert_eq!(
            app.nav_items().first().copied(),
            Some(NavItem::Chat {
                workspace: first_workspace,
                chat: first_chat,
            })
        );
        assert_eq!(app.selected_item(), app.nav_items().first().copied());
    }

    #[test]
    fn select_next_previous_wrap_around_the_nav_list() {
        let mut app = App::default();
        let items = app.nav_items();
        assert!(
            items.len() >= 2,
            "default seed should have multiple nav items"
        );

        app.select_item(items[0]);
        app.select_previous();
        assert_eq!(app.selected_item(), items.last().copied());
        app.select_next();
        assert_eq!(app.selected_item(), Some(items[0]));
    }

    #[test]
    fn deleting_the_selected_item_selects_the_successor_in_its_slot() {
        let mut app = App::default();
        let items = app.nav_items();
        assert!(
            items.len() >= 2,
            "default seed should have multiple nav items"
        );

        app.select_item(items[0]);
        assert_eq!(app.selected_item(), Some(items[0]));

        assert!(app.begin_delete_selected());
        app.confirm_delete();

        // The item that shifts into the vacated slot becomes selected (the old
        // second item), matching the position-stable behavior.
        assert_eq!(app.selected_item(), Some(items[1]));
    }

    #[test]
    fn deleting_last_workspace_item_closes_workspace() {
        let mut app = App::seeded();
        app.project.workspaces.truncate(1);
        app.project.workspaces[0].terminals.clear();
        app.project.workspaces[0].chats.truncate(1);
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });

        assert!(app.begin_delete_selected());
        let runtime_terminals = app.confirm_delete();

        assert_eq!(runtime_terminals, vec![PtyKey::ChatAgent(chat)]);
        assert!(app.project.workspace(workspace).is_none());
        assert!(app.is_dirty());
    }

    #[test]
    fn empty_workspace_can_be_closed_without_selecting_workspace() {
        let mut app = App::default();
        app.project.workspaces.truncate(1);
        app.project.workspaces[0].chats.clear();
        app.project.workspaces[0].terminals.clear();
        let workspace = app.project.workspaces[0].id;
        app.reconcile_selection(None);

        assert!(app.begin_delete_selected());
        let runtime_terminals = app.confirm_delete();

        assert!(runtime_terminals.is_empty());
        assert!(app.project.workspace(workspace).is_none());
        assert!(app.is_dirty());
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
    fn unchanged_status_updates_do_not_mark_dirty() {
        let mut app = App::seeded();
        let terminal = app.project.workspaces[0].terminals[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        let chat_status = app.project.workspaces[0].chats[0].status;
        app.mark_clean();

        app.mark_terminal_stopped(terminal);
        app.mark_chat_status_by_id(chat, chat_status);

        assert!(!app.is_dirty());
    }

    #[test]
    fn empty_selection_is_not_a_terminal_input_target() {
        let mut app = App::default();
        app.project.workspaces.clear();
        app.reconcile_selection(None);

        assert!(!app.begin_terminal_input());
        assert_eq!(app.pty_input_target(), None);
    }

    #[test]
    fn agent_message_event_appends_chat_transcript() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        let target = crate::agent::AgentTarget { workspace, chat };

        app.apply_agent_event(crate::agent::AgentEvent::MessageDelta {
            target,
            role: crate::agent::AgentMessageRole::Assistant,
            text: "hello".to_string(),
        });
        app.apply_agent_event(crate::agent::AgentEvent::MessageDelta {
            target,
            role: crate::agent::AgentMessageRole::Assistant,
            text: " world\nnext".to_string(),
        });

        assert_eq!(
            app.chat_lines(chat),
            vec![
                "agent > hello world".to_string(),
                "agent > next".to_string()
            ]
        );
        let messages = &app.project.chat(workspace, chat).unwrap().messages;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, ChatMessageRole::Assistant);
        assert_eq!(messages[0].text, "hello world\nnext");
        assert!(app.is_dirty());
    }

    #[test]
    fn app_hydrates_chat_transcript_from_project_state() {
        let mut state = ProjectState::seeded();
        let workspace = state.workspaces[0].id;
        let chat = state.workspaces[0].chats[0].id;
        state.append_chat_message(workspace, chat, ChatMessageRole::User, "hello".to_string());
        state.append_chat_message(
            workspace,
            chat,
            ChatMessageRole::Assistant,
            "hi there".to_string(),
        );

        let app = App::new(state);

        assert_eq!(
            app.chat_lines(chat),
            vec!["user > hello".to_string(), "agent > hi there".to_string()]
        );
        assert!(!app.is_dirty());
    }

    #[test]
    fn agent_status_and_error_events_update_chat_status() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        let target = crate::agent::AgentTarget { workspace, chat };

        app.apply_agent_event(crate::agent::AgentEvent::StatusChanged {
            target,
            status: ChatStatus::Done,
        });

        assert_eq!(
            app.project.chat(workspace, chat).unwrap().status,
            ChatStatus::Done
        );
        assert!(app.is_dirty());

        app.mark_clean();
        app.apply_agent_event(crate::agent::AgentEvent::Error {
            target,
            message: "backend failed".to_string(),
        });

        assert_eq!(
            app.project.chat(workspace, chat).unwrap().status,
            ChatStatus::Failed
        );
        assert_eq!(
            app.chat_lines(chat),
            vec!["error > backend failed".to_string()]
        );
        assert_eq!(
            app.project.chat(workspace, chat).unwrap().messages[0].role,
            ChatMessageRole::Error
        );
        assert!(app.is_dirty());
    }

    #[test]
    fn prompt_input_can_be_edited() {
        let mut app = App::default();
        app.begin_open_workspace(&[]);
        if let Some(Prompt::OpenWorkspace(prompt)) = &mut app.prompt {
            prompt.input = PromptInput::default();
        }

        app.push_prompt_char('/');
        app.push_prompt_char('t');
        app.pop_prompt_char();

        assert_eq!(
            app.prompt,
            Some(Prompt::OpenWorkspace(OpenWorkspacePrompt {
                input: PromptInput::new("/"),
                error: None,
                selected: ListSelection::default(),
                mode: OpenWorkspaceMode::Path,
            }))
        );
    }

    #[test]
    fn importing_workspace_adds_terminal_without_agent_chat() {
        let path = unique_temp_dir();
        let mut app = App::default();
        app.begin_open_workspace(&[]);
        if let Some(Prompt::OpenWorkspace(prompt)) = &mut app.prompt {
            prompt.input = PromptInput::new(path.display().to_string());
        }

        app.submit_open_workspace(&[]);

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.cwd.as_deref(), Some(path.as_path()));
        assert_eq!(imported.chats.len(), 0);
        assert_eq!(imported.terminals.len(), 1);
        assert_eq!(
            app.selected_item(),
            Some(NavItem::Terminal {
                workspace: imported.id,
                terminal: imported.terminals[0].id,
            })
        );
        assert_eq!(app.prompt, None);
        assert!(app.is_dirty());
    }

    #[test]
    fn configured_workspace_prompt_fuzzy_filters_by_name_and_uses_configured_name() {
        let selected_path = unique_temp_dir();
        let other_path = unique_temp_dir();
        let projects = vec![
            ConfiguredProject {
                name: "frontend".to_string(),
                path: other_path,
            },
            ConfiguredProject {
                name: "mult".to_string(),
                path: selected_path.clone(),
            },
        ];
        let mut app = App::default();

        app.begin_open_workspace(&projects);
        for ch in "mlt".chars() {
            app.push_prompt_char(ch);
        }

        let matches = app.open_workspace_matches(&projects);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "mult");

        app.submit_open_workspace(&projects);

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.name, "mult");
        assert_eq!(imported.cwd.as_deref(), Some(selected_path.as_path()));
        assert_eq!(
            app.selected_item(),
            Some(NavItem::Terminal {
                workspace: imported.id,
                terminal: imported.terminals[0].id,
            })
        );
        assert_eq!(app.prompt, None);
        assert!(app.is_dirty());
    }

    #[test]
    fn configured_workspace_prompt_arrow_selects_match() {
        let first_path = unique_temp_dir();
        let second_path = unique_temp_dir();
        let projects = vec![
            ConfiguredProject {
                name: "first".to_string(),
                path: first_path,
            },
            ConfiguredProject {
                name: "second".to_string(),
                path: second_path.clone(),
            },
        ];
        let mut app = App::default();

        app.begin_open_workspace(&projects);
        app.select_next_open_workspace_match(&projects);
        app.submit_open_workspace(&projects);

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.name, "second");
        assert_eq!(imported.cwd.as_deref(), Some(second_path.as_path()));
    }

    #[test]
    fn invalid_import_stays_in_prompt() {
        let mut app = App::default();
        app.begin_open_workspace(&[]);
        if let Some(Prompt::OpenWorkspace(prompt)) = &mut app.prompt {
            prompt.input = PromptInput::new("/this/path/should/not/exist");
        }

        app.submit_open_workspace(&[]);

        let Some(Prompt::OpenWorkspace(prompt)) = &app.prompt else {
            panic!("expected prompt to remain open");
        };
        assert_eq!(prompt.error.as_deref(), Some("path does not exist"));
        assert!(!app.is_dirty());
    }

    fn unique_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mult-test-{unique}"));
        fs::create_dir(&path).expect("create temp workspace");
        path.canonicalize().expect("canonicalize temp workspace")
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
        let Some(Prompt::CommandPalette(prompt)) = &app.prompt else {
            panic!("palette is open");
        };
        assert_eq!(prompt.selected.index(), 2);

        app.apply_prompt_edit(PromptEdit::MoveHome);
        let Some(Prompt::CommandPalette(prompt)) = &app.prompt else {
            panic!("palette is open");
        };
        assert_eq!(prompt.selected.index(), 2, "a motion is not a new query");

        app.push_prompt_char('f');
        let Some(Prompt::CommandPalette(prompt)) = &app.prompt else {
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

    // ---- E2: the status surface -------------------------------------------

    #[test]
    fn notices_are_transient_deduplicated_and_dismissible() {
        let mut app = App::default();
        let now = Instant::now();

        assert!(app.push_notice_at(now, NoticeLevel::Error, NoticeSource::Report, "daemon gone"));
        // A failure that repeats every retry frame refreshes its row rather
        // than filling the surface with copies of one sentence.
        assert!(!app.push_notice_at(now, NoticeLevel::Error, NoticeSource::Report, "daemon gone"));
        assert_eq!(app.notices().len(), 1);
        assert_eq!(app.notices()[0].text(), "daemon gone");
        assert_eq!(app.notices()[0].level(), NoticeLevel::Error);

        // Still there just before the deadline, gone at it: the surface does
        // not permanently steal a row.
        assert!(!app.expire_notices(now + NOTICE_TTL - Duration::from_millis(1)));
        assert_eq!(app.notices().len(), 1);
        assert!(app.expire_notices(now + NOTICE_TTL));
        assert!(app.notices().is_empty());
        assert!(!app.expire_notices(now + NOTICE_TTL));

        // Dismissal does not wait for the deadline.
        app.push_notice_at(
            now,
            NoticeLevel::Info,
            NoticeSource::Report,
            "config reloaded",
        );
        assert!(app.dismiss_notices());
        assert!(app.notices().is_empty());
        assert!(!app.dismiss_notices());
    }

    #[test]
    fn the_notice_surface_is_bounded() {
        let mut app = App::default();
        let now = Instant::now();
        for index in 0..MAX_NOTICES + 3 {
            app.push_notice_at(
                now,
                NoticeLevel::Warning,
                NoticeSource::Report,
                format!("notice {index}"),
            );
        }

        assert_eq!(app.notices().len(), MAX_NOTICES);
        // The oldest are the ones dropped.
        assert_eq!(app.notices()[0].text(), "notice 3");
    }

    #[test]
    fn a_save_failure_notice_sticks_until_a_save_succeeds() {
        let mut app = App::default();
        let now = Instant::now();
        app.record_save_failure("disk full");

        assert_eq!(app.save_error(), Some("disk full"));
        assert_eq!(app.notices().len(), 1);
        assert!(app.notices()[0]
            .text()
            .contains("State save failed: disk full"));
        // It describes a condition that is still true, so time does not clear
        // it the way it clears an event.
        assert!(!app.expire_notices(now + NOTICE_TTL * 100));
        assert_eq!(app.notices().len(), 1);

        app.mark_saved();
        assert_eq!(app.save_error(), None);
        assert!(app.notices().is_empty());
    }

    #[test]
    fn a_failed_operation_is_reported_and_retracted_by_the_next_success() {
        let mut app = App::default();
        app.project.workspaces.clear();
        app.select_nav_index(0);

        // No workspace to add a terminal to.
        app.add_terminal_to_selected_workspace();
        assert_eq!(app.notices().len(), 0, "no workspace means no attempt");

        let mut app = App::default();
        app.record_operation_failure("selected workspace no longer exists");
        assert_eq!(app.notices().len(), 1);
        assert!(app.notices()[0]
            .text()
            .starts_with("Operation failed: selected workspace"));

        app.add_terminal_to_selected_workspace();
        assert!(
            app.notices().is_empty(),
            "a successful mutation retracts the previous failure"
        );
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
            "Ctrl+f",
            "Ctrl+q",
            "? or F1",
        ] {
            assert!(keys.contains(&expected), "{expected} is not in the table");
        }
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
    fn dismiss_notices_is_only_offered_while_there_is_something_to_dismiss() {
        let mut app = App::default();
        let has_dismiss = |app: &App| {
            app.command_palette_entries_for("")
                .iter()
                .any(|entry| entry.action == CommandAction::DismissNotices)
        };

        assert!(!has_dismiss(&app));
        app.push_notice(
            NoticeLevel::Info,
            NoticeSource::Report,
            "something happened",
        );
        assert!(has_dismiss(&app));
    }

    #[test]
    fn a_config_reload_request_is_taken_exactly_once() {
        let mut app = App::default();
        assert!(!app.take_config_reload_request());

        app.request_config_reload();
        assert!(app.take_config_reload_request());
        assert!(!app.take_config_reload_request());
    }
}
