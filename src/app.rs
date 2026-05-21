use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
};

use crate::{
    agent::{AgentEvent, AgentMessageRole, AgentTarget},
    model::{
        ChatId, ChatMessage, ChatMessageRole, ChatStatus, ProjectState, TerminalId, TerminalStatus,
        WorkspaceId, DEFAULT_AGENT_CHAT_TITLE, RUNTIME_TERMINAL_ID_FLAG,
    },
};

pub use mult_protocol::{
    bounded_screen_dimensions, Cursor, ScreenSnapshot, ScreenUpdate, TerminalCell,
    TerminalCellStyle, TerminalColor, TerminalRenderLine, TerminalRenderSpan,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub project: ProjectState,
    pub selected: usize,
    pub mode: Mode,
    pub prompt: Option<Prompt>,
    pub focus: FocusMode,
    pub terminal_buffers: BTreeMap<TerminalId, TerminalBuffer>,
    pub terminal_snapshots: BTreeMap<TerminalId, ScreenSnapshot>,
    pub chat_buffers: BTreeMap<ChatId, ChatBuffer>,
    pub active_search: Option<SearchState>,
    pub should_quit: bool,
    dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Input(InputTarget),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputTarget {
    Terminal {
        workspace: WorkspaceId,
        terminal: TerminalId,
    },
    ChatAgent {
        workspace: WorkspaceId,
        chat: ChatId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    OpenWorkspace(OpenWorkspacePrompt),
    NewTerminalCommand(TerminalCommandPrompt),
    CommandPalette(CommandPalettePrompt),
    Search(SearchPrompt),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusMode {
    #[default]
    Sidebar,
    Chat,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWorkspacePrompt {
    pub input: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommandPrompt {
    pub input: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPalettePrompt {
    pub input: String,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPrompt {
    pub input: String,
    pub scope: SearchScope,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub scope: SearchScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Terminal(TerminalId),
    Chat(ChatId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAction {
    FocusSidebar,
    FocusSelectedPane,
    StartInput,
    AddAgentChat,
    AddShellTerminal,
    AddCommandTerminal,
    OpenWorkspace,
    DeleteSelected,
    SearchSelectedPane,
    ClearSearch,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandPaletteEntry {
    pub action: CommandAction,
    pub label: &'static str,
    pub help: &'static str,
}

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalBuffer {
    screen: TerminalScreen,
    parser: TerminalParser,
    scroll_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalScreen {
    rows: u16,
    cols: u16,
    cursor_row: usize,
    cursor_col: usize,
    cursor_visible: bool,
    wrap_pending: bool,
    saved_cursor: Option<(usize, usize)>,
    current_style: TerminalCellStyle,
    application_cursor_keys: bool,
    bracketed_paste: bool,
    scroll_top: usize,
    scroll_bottom: usize,
    scrollback: VecDeque<Vec<TerminalCell>>,
    cells: Vec<Vec<TerminalCell>>,
    alternate_saved: Option<TerminalScreenState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalScreenState {
    cursor_row: usize,
    cursor_col: usize,
    cursor_visible: bool,
    wrap_pending: bool,
    saved_cursor: Option<(usize, usize)>,
    current_style: TerminalCellStyle,
    scroll_top: usize,
    scroll_bottom: usize,
    scrollback: VecDeque<Vec<TerminalCell>>,
    cells: Vec<Vec<TerminalCell>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum TerminalParser {
    #[default]
    Ground,
    Escape,
    Csi(String),
    CsiIgnored,
    Osc {
        esc_seen: bool,
    },
    IgnoreOne,
}

const TERMINAL_MAX_SCROLLBACK_LINES: usize = 5_000;
const TERMINAL_MAX_CSI_SEQUENCE_CHARS: usize = 128;
const PRIMARY_DEVICE_ATTRIBUTES_RESPONSE: &[u8] = b"\x1b[?1;2c";
const DEVICE_STATUS_OK_RESPONSE: &[u8] = b"\x1b[0n";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatBuffer {
    lines: Vec<String>,
    partial: String,
    partial_role: Option<ChatMessageRole>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavItem {
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

impl Default for App {
    fn default() -> Self {
        Self::new(ProjectState::default())
    }
}

pub fn chat_agent_terminal_id(chat: ChatId) -> TerminalId {
    TerminalId(chat.0 | RUNTIME_TERMINAL_ID_FLAG)
}

pub fn chat_id_from_agent_terminal_id(terminal: TerminalId) -> Option<ChatId> {
    ((terminal.0 & RUNTIME_TERMINAL_ID_FLAG) != 0)
        .then_some(ChatId(terminal.0 & !RUNTIME_TERMINAL_ID_FLAG))
}

impl App {
    pub fn new(mut project: ProjectState) -> Self {
        let ids_normalized = project.normalize_next_ids();
        let titles_normalized = normalize_agent_chat_titles(&mut project);
        let chat_buffers = project
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter())
            .map(|chat| (chat.id, ChatBuffer::from_messages(&chat.messages)))
            .filter(|(_, buffer)| !buffer.is_empty())
            .collect();
        let mut app = Self {
            project,
            selected: 0,
            mode: Mode::Normal,
            prompt: None,
            focus: FocusMode::Sidebar,
            terminal_buffers: BTreeMap::new(),
            terminal_snapshots: BTreeMap::new(),
            chat_buffers,
            active_search: None,
            should_quit: false,
            dirty: ids_normalized || titles_normalized,
        };
        app.clamp_selection();
        app
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn is_prompt_active(&self) -> bool {
        self.prompt.is_some()
    }

    pub fn begin_command_palette(&mut self) {
        self.prompt = Some(Prompt::CommandPalette(CommandPalettePrompt {
            input: String::new(),
            selected: 0,
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
        let Some(Prompt::CommandPalette(prompt)) = &self.prompt else {
            return None;
        };
        let entries = self.command_palette_entries_for(&prompt.input);
        let action = entries
            .get(prompt.selected.min(entries.len().saturating_sub(1)))
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
            input,
            scope,
            error: None,
        }));
        true
    }

    pub fn submit_search(&mut self) {
        let Some(Prompt::Search(prompt)) = &self.prompt else {
            return;
        };
        let query = prompt.input.trim().to_string();
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

    pub fn search_status(&self) -> Option<String> {
        let search = self.active_search.as_ref()?;
        let count = self.search_match_count(search);
        let scope = match search.scope {
            SearchScope::Terminal(_) => "terminal",
            SearchScope::Chat(_) => "chat",
        };
        Some(format!(
            "search {scope}: {count} match{} for `{}`",
            if count == 1 { "" } else { "es" },
            search.query
        ))
    }

    #[cfg(test)]
    pub fn focus_next(&mut self) {
        self.cycle_focus(false);
    }

    #[cfg(test)]
    pub fn focus_previous(&mut self) {
        self.cycle_focus(true);
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

    #[cfg(test)]
    fn cycle_focus(&mut self, backwards: bool) {
        let available = self.available_focus_modes();
        if available.is_empty() {
            self.focus = FocusMode::Sidebar;
            return;
        }

        let index = available
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = if backwards {
            index.checked_sub(1).unwrap_or(available.len() - 1)
        } else {
            (index + 1) % available.len()
        };
        self.focus = available[next];
    }

    fn available_focus_modes(&self) -> Vec<FocusMode> {
        let mut modes = vec![FocusMode::Sidebar];
        if let Some(focus) = self.selected_main_focus() {
            modes.push(focus);
        }
        modes
    }

    fn selected_main_focus(&self) -> Option<FocusMode> {
        match self.selected_item()? {
            NavItem::Chat { .. } => Some(FocusMode::Chat),
            NavItem::Terminal { .. } => Some(FocusMode::Terminal),
            NavItem::Workspace(_) => None,
        }
    }

    fn normalize_focus(&mut self) {
        if !self.available_focus_modes().contains(&self.focus) {
            self.focus = FocusMode::Sidebar;
        }
    }

    fn available_command_palette_entries(&self) -> Vec<CommandPaletteEntry> {
        let mut entries = Vec::new();
        entries.push(CommandPaletteEntry {
            action: CommandAction::FocusSidebar,
            label: "Focus sidebar",
            help: "return keyboard focus to workspace navigation",
        });
        if self.selected_main_focus().is_some() {
            entries.push(CommandPaletteEntry {
                action: CommandAction::FocusSelectedPane,
                label: "Focus selected pane",
                help: "move keyboard focus from sidebar to the selected chat or terminal",
            });
            entries.push(CommandPaletteEntry {
                action: CommandAction::StartInput,
                label: "Start or focus input",
                help: "start the selected chat/terminal PTY and enter input mode",
            });
            entries.push(CommandPaletteEntry {
                action: CommandAction::SearchSelectedPane,
                label: "Search selected pane",
                help: "filter terminal output or chat transcript lines",
            });
        }
        if self.selected_workspace_id().is_some() {
            entries.push(CommandPaletteEntry {
                action: CommandAction::AddAgentChat,
                label: "New agent chat",
                help: "add an agent chat to the selected workspace",
            });
            entries.push(CommandPaletteEntry {
                action: CommandAction::AddShellTerminal,
                label: "New shell terminal",
                help: "add a shell terminal to the selected workspace",
            });
            entries.push(CommandPaletteEntry {
                action: CommandAction::AddCommandTerminal,
                label: "New command terminal",
                help: "add a command/dev-server terminal to the selected workspace",
            });
        }
        entries.push(CommandPaletteEntry {
            action: CommandAction::OpenWorkspace,
            label: "Open workspace",
            help: "import a workspace directory",
        });
        if self.selected_item_can_be_deleted() {
            entries.push(CommandPaletteEntry {
                action: CommandAction::DeleteSelected,
                label: "Delete selected item",
                help: "delete the selected workspace, chat, or terminal",
            });
        }
        if self.active_search.is_some() {
            entries.push(CommandPaletteEntry {
                action: CommandAction::ClearSearch,
                label: "Clear search",
                help: "clear the active search/filter",
            });
        }
        entries.push(CommandPaletteEntry {
            action: CommandAction::Quit,
            label: "Quit mult",
            help: "save state and exit",
        });
        entries
    }

    fn move_command_palette_selection(&mut self, delta: isize) {
        let Some(Prompt::CommandPalette(prompt)) = &self.prompt else {
            return;
        };
        let len = self.command_palette_entries_for(&prompt.input).len();
        if len == 0 {
            if let Some(Prompt::CommandPalette(prompt)) = &mut self.prompt {
                prompt.selected = 0;
            }
            return;
        }

        if let Some(Prompt::CommandPalette(prompt)) = &mut self.prompt {
            if delta.is_negative() {
                let delta = delta.unsigned_abs() % len;
                prompt.selected = prompt.selected.checked_sub(delta).unwrap_or(len - delta);
            } else {
                prompt.selected = (prompt.selected + delta as usize) % len;
            }
        }
    }

    fn clamp_command_palette_selection(&mut self) {
        let Some(Prompt::CommandPalette(prompt)) = &self.prompt else {
            return;
        };
        let len = self.command_palette_entries_for(&prompt.input).len();
        if let Some(Prompt::CommandPalette(prompt)) = &mut self.prompt {
            prompt.selected = prompt.selected.min(len.saturating_sub(1));
        }
    }

    fn search_match_count(&self, search: &SearchState) -> usize {
        match search.scope {
            SearchScope::Terminal(terminal) => {
                filter_lines(self.terminal_all_lines(terminal), &search.query).len()
            }
            SearchScope::Chat(chat) => {
                filter_lines(self.chat_transcript_lines(chat), &search.query).len()
            }
        }
    }

    pub fn terminal_input_target(&self) -> Option<TerminalId> {
        match self.mode {
            Mode::Input(InputTarget::Terminal { terminal, .. }) => Some(terminal),
            _ => None,
        }
    }

    pub fn pty_input_target(&self) -> Option<TerminalId> {
        match self.mode {
            Mode::Input(InputTarget::Terminal { terminal, .. }) => Some(terminal),
            Mode::Input(InputTarget::ChatAgent { chat, .. }) => Some(chat_agent_terminal_id(chat)),
            Mode::Normal => None,
        }
    }

    pub fn nav_items(&self) -> Vec<NavItem> {
        let mut items = Vec::with_capacity(self.nav_len());

        for workspace in &self.project.workspaces {
            items.push(NavItem::Workspace(workspace.id));

            for chat in &workspace.chats {
                items.push(NavItem::Chat {
                    workspace: workspace.id,
                    chat: chat.id,
                });
            }

            for terminal in &workspace.terminals {
                items.push(NavItem::Terminal {
                    workspace: workspace.id,
                    terminal: terminal.id,
                });
            }
        }

        items
    }

    pub fn nav_len(&self) -> usize {
        self.project
            .workspaces
            .iter()
            .map(|workspace| 1 + workspace.chats.len() + workspace.terminals.len())
            .sum()
    }

    pub fn nav_item_at(&self, target_index: usize) -> Option<NavItem> {
        let mut index = 0;
        for workspace in &self.project.workspaces {
            if index == target_index {
                return Some(NavItem::Workspace(workspace.id));
            }
            index += 1;

            for chat in &workspace.chats {
                if index == target_index {
                    return Some(NavItem::Chat {
                        workspace: workspace.id,
                        chat: chat.id,
                    });
                }
                index += 1;
            }

            for terminal in &workspace.terminals {
                if index == target_index {
                    return Some(NavItem::Terminal {
                        workspace: workspace.id,
                        terminal: terminal.id,
                    });
                }
                index += 1;
            }
        }

        None
    }

    pub fn selected_item(&self) -> Option<NavItem> {
        self.nav_item_at(self.selected)
    }

    pub fn selected_workspace_id(&self) -> Option<WorkspaceId> {
        match self.selected_item() {
            Some(NavItem::Workspace(workspace))
            | Some(NavItem::Chat { workspace, .. })
            | Some(NavItem::Terminal { workspace, .. }) => Some(workspace),
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
            NavItem::Workspace(_) => None,
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

    pub fn delete_selected_immediately(&mut self) -> Vec<TerminalId> {
        let Some(target) = self.selected_delete_target() else {
            return Vec::new();
        };
        self.prompt = None;
        self.delete_target(target)
    }

    fn delete_target(&mut self, target: DeleteTarget) -> Vec<TerminalId> {
        let mut runtime_terminals = Vec::new();
        match target {
            DeleteTarget::Workspace(workspace_id) => {
                if let Some(workspace) = self.project.remove_workspace(workspace_id) {
                    runtime_terminals
                        .extend(workspace.terminals.iter().map(|terminal| terminal.id));
                    runtime_terminals.extend(
                        workspace
                            .chats
                            .iter()
                            .map(|chat| chat_agent_terminal_id(chat.id)),
                    );
                    for terminal in &runtime_terminals {
                        self.terminal_buffers.remove(terminal);
                        self.terminal_snapshots.remove(terminal);
                    }
                    for chat in workspace.chats {
                        self.chat_buffers.remove(&chat.id);
                    }
                    self.dirty = true;
                }
            }
            DeleteTarget::Chat { workspace, chat } => {
                if self.project.remove_chat(workspace, chat).is_some() {
                    let terminal = chat_agent_terminal_id(chat);
                    runtime_terminals.push(terminal);
                    self.terminal_buffers.remove(&terminal);
                    self.terminal_snapshots.remove(&terminal);
                    self.chat_buffers.remove(&chat);
                    self.dirty = true;
                }
            }
            DeleteTarget::Terminal {
                workspace,
                terminal,
            } => {
                if self.project.remove_terminal(workspace, terminal).is_some() {
                    runtime_terminals.push(terminal);
                    self.terminal_buffers.remove(&terminal);
                    self.terminal_snapshots.remove(&terminal);
                    self.dirty = true;
                }
            }
        }

        self.clamp_selection();
        self.normalize_focus();
        runtime_terminals
    }

    fn selected_delete_target(&self) -> Option<DeleteTarget> {
        match self.selected_item()? {
            NavItem::Workspace(workspace) => Some(DeleteTarget::Workspace(workspace)),
            NavItem::Chat { workspace, chat } => Some(DeleteTarget::Chat { workspace, chat }),
            NavItem::Terminal {
                workspace,
                terminal,
            } => Some(DeleteTarget::Terminal {
                workspace,
                terminal,
            }),
        }
    }

    pub fn mark_terminal_running(&mut self, terminal: TerminalId) {
        if let Some(terminal) = self.project.terminal_mut_by_id(terminal) {
            if terminal.status != TerminalStatus::Running {
                terminal.status = TerminalStatus::Running;
                self.dirty = true;
            }
        }
    }

    pub fn mark_terminal_stopped(&mut self, terminal: TerminalId) {
        if let Some(terminal) = self.project.terminal_mut_by_id(terminal) {
            if terminal.status != TerminalStatus::Stopped {
                terminal.status = TerminalStatus::Stopped;
                self.dirty = true;
            }
        }
    }

    pub fn append_terminal_output(&mut self, terminal: TerminalId, text: &str) {
        self.terminal_snapshots.remove(&terminal);
        self.terminal_buffers
            .entry(terminal)
            .or_default()
            .append(text);
    }

    pub fn set_terminal_snapshot(&mut self, terminal: TerminalId, snapshot: ScreenSnapshot) {
        self.terminal_snapshots.insert(terminal, snapshot);
    }

    pub fn apply_terminal_update(&mut self, terminal: TerminalId, update: ScreenUpdate) {
        self.terminal_snapshots
            .entry(terminal)
            .or_insert_with(|| ScreenSnapshot::blank(update.rows, update.cols))
            .apply_update(update);
    }

    pub fn append_terminal_system_line(
        &mut self,
        terminal: TerminalId,
        message: impl Into<String>,
    ) {
        self.terminal_snapshots.remove(&terminal);
        self.terminal_buffers
            .entry(terminal)
            .or_default()
            .push_line(format!("[mult] {}", message.into()));
    }

    pub fn terminal_lines(&self, terminal: TerminalId) -> Vec<String> {
        self.terminal_buffers
            .get(&terminal)
            .map(TerminalBuffer::visible_lines)
            .unwrap_or_default()
    }

    pub fn terminal_all_lines(&self, terminal: TerminalId) -> Vec<String> {
        self.terminal_snapshots
            .get(&terminal)
            .map(screen_snapshot_lines)
            .or_else(|| {
                self.terminal_buffers
                    .get(&terminal)
                    .map(TerminalBuffer::all_lines)
            })
            .unwrap_or_default()
    }

    pub fn filtered_terminal_lines(&self, terminal: TerminalId) -> Option<Vec<String>> {
        let search = self.active_search.as_ref()?;
        if search.scope != SearchScope::Terminal(terminal) {
            return None;
        }
        Some(filter_lines(
            self.terminal_all_lines(terminal),
            &search.query,
        ))
    }

    pub fn active_search_query_for_terminal(&self, terminal: TerminalId) -> Option<&str> {
        self.active_search
            .as_ref()
            .filter(|search| search.scope == SearchScope::Terminal(terminal))
            .map(|search| search.query.as_str())
    }

    pub fn terminal_render_lines(&self, terminal: TerminalId) -> Vec<TerminalRenderLine> {
        let force_cursor_visible = self.pty_input_target() == Some(terminal);
        if let Some(snapshot) = self.terminal_snapshots.get(&terminal) {
            let mut snapshot = snapshot.clone();
            if force_cursor_visible {
                if let Some(cursor) = snapshot.cursor.as_mut() {
                    cursor.visible = true;
                }
            }
            return snapshot.render_lines();
        }

        self.terminal_buffers
            .get(&terminal)
            .map(|buffer| buffer.render_lines_with_forced_cursor(force_cursor_visible))
            .unwrap_or_default()
    }

    pub fn terminal_output_is_blank(&self, terminal: TerminalId) -> bool {
        self.terminal_snapshots
            .get(&terminal)
            .map(ScreenSnapshot::is_blank)
            .or_else(|| {
                self.terminal_buffers
                    .get(&terminal)
                    .map(TerminalBuffer::is_blank)
            })
            .unwrap_or(true)
    }

    pub fn selected_output_terminal_id(&self) -> Option<TerminalId> {
        match self.selected_item()? {
            NavItem::Chat { chat, .. } => Some(chat_agent_terminal_id(chat)),
            NavItem::Terminal { terminal, .. } => Some(terminal),
            NavItem::Workspace(_) => None,
        }
    }

    pub fn scroll_selected_output_up(&mut self, rows: usize) -> bool {
        let Some(terminal) = self.selected_output_terminal_id() else {
            return false;
        };
        self.scroll_terminal_output_up(terminal, rows)
    }

    pub fn scroll_selected_output_down(&mut self, rows: usize) -> bool {
        let Some(terminal) = self.selected_output_terminal_id() else {
            return false;
        };
        self.scroll_terminal_output_down(terminal, rows)
    }

    pub fn scroll_selected_output_to_top(&mut self) -> bool {
        let Some(terminal) = self.selected_output_terminal_id() else {
            return false;
        };
        self.scroll_terminal_output_to_top(terminal)
    }

    pub fn scroll_selected_output_to_bottom(&mut self) -> bool {
        let Some(terminal) = self.selected_output_terminal_id() else {
            return false;
        };
        self.scroll_terminal_output_to_bottom(terminal)
    }

    pub fn scroll_terminal_output_up(&mut self, terminal: TerminalId, rows: usize) -> bool {
        let Some(buffer) = self.terminal_buffers.get_mut(&terminal) else {
            return false;
        };
        buffer.scroll_view_up(rows)
    }

    pub fn scroll_terminal_output_down(&mut self, terminal: TerminalId, rows: usize) -> bool {
        let Some(buffer) = self.terminal_buffers.get_mut(&terminal) else {
            return false;
        };
        buffer.scroll_view_down(rows)
    }

    pub fn scroll_terminal_output_to_top(&mut self, terminal: TerminalId) -> bool {
        let Some(buffer) = self.terminal_buffers.get_mut(&terminal) else {
            return false;
        };
        buffer.scroll_view_to_top()
    }

    pub fn scroll_terminal_output_to_bottom(&mut self, terminal: TerminalId) -> bool {
        let Some(buffer) = self.terminal_buffers.get_mut(&terminal) else {
            return false;
        };
        buffer.scroll_view_to_bottom()
    }

    pub fn resize_terminal_buffer(&mut self, terminal: TerminalId, rows: u16, cols: u16) {
        self.terminal_buffers
            .entry(terminal)
            .or_default()
            .resize(rows, cols);
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
                if let Some(chat) = self.project.chat_mut(target.workspace, target.chat) {
                    if chat.status != status {
                        chat.status = status;
                        self.dirty = true;
                    }
                }
            }
            AgentEvent::Error { target, message } => {
                if let Some(chat) = self.project.chat_mut(target.workspace, target.chat) {
                    if chat.status != ChatStatus::Failed {
                        chat.status = ChatStatus::Failed;
                        self.dirty = true;
                    }
                }
                self.append_chat_message(target, ChatMessageRole::Error, message);
            }
        }
    }

    pub fn select_next(&mut self) {
        let len = self.nav_len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
            self.normalize_focus();
        }
    }

    pub fn select_previous(&mut self) {
        let len = self.nav_len();
        if len > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(len - 1);
            self.normalize_focus();
        }
    }

    pub fn begin_open_workspace(&mut self) {
        let input = std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        self.prompt = Some(Prompt::OpenWorkspace(OpenWorkspacePrompt {
            input,
            error: None,
        }));
    }

    pub fn begin_new_terminal_command(&mut self) -> bool {
        if self.selected_workspace_id().is_none() {
            return false;
        }

        self.prompt = Some(Prompt::NewTerminalCommand(TerminalCommandPrompt {
            input: String::new(),
            error: None,
        }));
        true
    }

    pub fn cancel_prompt(&mut self) {
        self.prompt = None;
    }

    pub fn begin_terminal_input(&mut self) -> bool {
        let Some((workspace, terminal)) = self.selected_terminal_id() else {
            return false;
        };

        self.focus = FocusMode::Terminal;
        self.mode = Mode::Input(InputTarget::Terminal {
            workspace,
            terminal,
        });
        true
    }

    pub fn begin_chat_agent_input(&mut self) -> bool {
        let Some((workspace, chat)) = self.selected_chat_id() else {
            return false;
        };

        self.focus = FocusMode::Chat;
        self.mode = Mode::Input(InputTarget::ChatAgent { workspace, chat });
        true
    }

    pub fn end_terminal_input(&mut self) {
        self.mode = Mode::Normal;
        self.focus = FocusMode::Sidebar;
    }

    pub fn end_pty_input(&mut self) {
        self.mode = Mode::Normal;
        self.focus = FocusMode::Sidebar;
    }

    pub fn mark_chat_status_by_id(&mut self, chat: ChatId, status: ChatStatus) {
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
                }
                return;
            }
        }
    }

    pub fn push_prompt_char(&mut self, c: char) {
        match &mut self.prompt {
            Some(Prompt::OpenWorkspace(prompt)) => {
                prompt.input.push(c);
                prompt.error = None;
            }
            Some(Prompt::NewTerminalCommand(prompt)) => {
                prompt.input.push(c);
                prompt.error = None;
            }
            Some(Prompt::CommandPalette(prompt)) => {
                prompt.input.push(c);
                prompt.selected = 0;
            }
            Some(Prompt::Search(prompt)) => {
                prompt.input.push(c);
                prompt.error = None;
            }
            _ => {}
        }
        self.clamp_command_palette_selection();
    }

    pub fn pop_prompt_char(&mut self) {
        match &mut self.prompt {
            Some(Prompt::OpenWorkspace(prompt)) => {
                prompt.input.pop();
                prompt.error = None;
            }
            Some(Prompt::NewTerminalCommand(prompt)) => {
                prompt.input.pop();
                prompt.error = None;
            }
            Some(Prompt::CommandPalette(prompt)) => {
                prompt.input.pop();
                prompt.selected = 0;
            }
            Some(Prompt::Search(prompt)) => {
                prompt.input.pop();
                prompt.error = None;
            }
            _ => {}
        }
        self.clamp_command_palette_selection();
    }

    pub fn submit_open_workspace(&mut self) {
        let Some(Prompt::OpenWorkspace(prompt)) = &self.prompt else {
            return;
        };
        let raw_input = prompt.input.trim();
        if raw_input.is_empty() {
            self.set_open_workspace_error("enter a directory path");
            return;
        }

        let path = expand_tilde(raw_input);
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
            self.prompt = None;
            self.select_item(NavItem::Workspace(existing_workspace.id));
            return;
        }

        let name = workspace_name(&cwd);
        let workspace = self.project.add_workspace(name.clone(), Some(cwd));
        self.project.add_chat(
            workspace,
            DEFAULT_AGENT_CHAT_TITLE.to_string(),
            ChatStatus::Idle,
        );
        self.project
            .add_terminal(workspace, "shell".to_string(), TerminalStatus::Stopped);

        self.prompt = None;
        self.select_item(NavItem::Workspace(workspace));
        self.dirty = true;
    }

    pub fn submit_new_terminal_command(&mut self) {
        let Some(Prompt::NewTerminalCommand(prompt)) = &self.prompt else {
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

        if let Some(terminal) = self.project.add_command_terminal(
            workspace,
            name.clone(),
            TerminalStatus::Stopped,
            command,
        ) {
            self.prompt = None;
            self.select_item(NavItem::Terminal {
                workspace,
                terminal,
            });
            self.dirty = true;
        }
    }

    pub fn add_chat_to_selected_workspace_and_return(&mut self) -> Option<(WorkspaceId, ChatId)> {
        let workspace = self.selected_workspace_id()?;
        let name = DEFAULT_AGENT_CHAT_TITLE.to_string();
        let chat = self.project.add_chat(workspace, name, ChatStatus::Idle)?;
        self.select_item(NavItem::Chat { workspace, chat });
        self.dirty = true;
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
        if let Some(terminal) =
            self.project
                .add_terminal(workspace, name.clone(), TerminalStatus::Stopped)
        {
            self.select_item(NavItem::Terminal {
                workspace,
                terminal,
            });
            self.dirty = true;
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

    fn select_item(&mut self, target: NavItem) {
        if let Some(index) = self.nav_item_position(target) {
            self.selected = index;
        } else {
            self.clamp_selection();
        }
        self.normalize_focus();
    }

    fn nav_item_position(&self, target: NavItem) -> Option<usize> {
        let mut index = 0;
        for workspace in &self.project.workspaces {
            if NavItem::Workspace(workspace.id) == target {
                return Some(index);
            }
            index += 1;

            for chat in &workspace.chats {
                if (NavItem::Chat {
                    workspace: workspace.id,
                    chat: chat.id,
                }) == target
                {
                    return Some(index);
                }
                index += 1;
            }

            for terminal in &workspace.terminals {
                if (NavItem::Terminal {
                    workspace: workspace.id,
                    terminal: terminal.id,
                }) == target
                {
                    return Some(index);
                }
                index += 1;
            }
        }

        None
    }

    fn clamp_selection(&mut self) {
        let len = self.nav_len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

impl TerminalBuffer {
    pub fn append(&mut self, text: &str) {
        let _ = self.append_and_collect_responses(text);
    }

    pub fn append_and_collect_responses(&mut self, text: &str) -> Vec<Vec<u8>> {
        let old_max_scroll_offset = self.screen.max_scroll_offset();
        let preserve_scrolled_view = self.scroll_offset > 0;
        let mut responses = Vec::new();
        for ch in text.chars() {
            self.process_char(ch, &mut responses);
        }
        if preserve_scrolled_view {
            self.scroll_offset = self.scroll_offset.saturating_add(
                self.screen
                    .max_scroll_offset()
                    .saturating_sub(old_max_scroll_offset),
            );
        }
        self.clamp_scroll_offset();
        responses
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.screen
            .resize(rows.max(1), cols.max(1), self.scroll_offset == 0);
        self.clamp_scroll_offset();
    }

    fn visible_lines(&self) -> Vec<String> {
        self.screen.visible_lines(self.scroll_offset)
    }

    pub fn all_lines(&self) -> Vec<String> {
        self.screen.all_lines()
    }

    pub fn render_lines(&self) -> Vec<TerminalRenderLine> {
        self.screen.render_lines(self.scroll_offset)
    }

    fn render_lines_with_forced_cursor(
        &self,
        force_cursor_visible: bool,
    ) -> Vec<TerminalRenderLine> {
        self.screen
            .render_lines_with_forced_cursor(self.scroll_offset, force_cursor_visible)
    }

    pub fn snapshot(&self) -> ScreenSnapshot {
        self.screen.snapshot(self.scroll_offset)
    }

    pub fn is_blank(&self) -> bool {
        self.screen.is_blank()
    }

    pub fn application_cursor_keys_enabled(&self) -> bool {
        self.screen.application_cursor_keys
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.screen.bracketed_paste
    }

    pub fn paste_bytes(&self, text: &str) -> Vec<u8> {
        terminal_paste_bytes(text, self.screen.bracketed_paste)
    }

    pub fn scroll_view_up(&mut self, rows: usize) -> bool {
        if rows == 0 {
            return false;
        }
        let old_offset = self.scroll_offset;
        self.scroll_offset = (self.scroll_offset + rows).min(self.screen.max_scroll_offset());
        self.scroll_offset != old_offset
    }

    pub fn scroll_view_down(&mut self, rows: usize) -> bool {
        if rows == 0 {
            return false;
        }
        let old_offset = self.scroll_offset;
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.scroll_offset != old_offset
    }

    pub fn scroll_view_to_top(&mut self) -> bool {
        let old_offset = self.scroll_offset;
        self.scroll_offset = self.screen.max_scroll_offset();
        self.scroll_offset != old_offset
    }

    pub fn scroll_view_to_bottom(&mut self) -> bool {
        let old_offset = self.scroll_offset;
        self.scroll_offset = 0;
        self.scroll_offset != old_offset
    }

    fn clamp_scroll_offset(&mut self) {
        self.scroll_offset = self.scroll_offset.min(self.screen.max_scroll_offset());
    }

    fn push_line(&mut self, mut line: String) {
        line.push('\r');
        line.push('\n');
        self.append(&line);
    }

    fn process_char(&mut self, ch: char, responses: &mut Vec<Vec<u8>>) {
        let state = std::mem::take(&mut self.parser);
        self.parser = match state {
            TerminalParser::Ground => self.process_ground_char(ch),
            TerminalParser::Escape => self.process_escape_char(ch, responses),
            TerminalParser::Csi(mut sequence) => {
                if ('@'..='~').contains(&ch) {
                    self.apply_csi(&sequence, ch, responses);
                    TerminalParser::Ground
                } else if sequence.len() >= TERMINAL_MAX_CSI_SEQUENCE_CHARS {
                    TerminalParser::CsiIgnored
                } else {
                    sequence.push(ch);
                    TerminalParser::Csi(sequence)
                }
            }
            TerminalParser::CsiIgnored => {
                if ('@'..='~').contains(&ch) {
                    TerminalParser::Ground
                } else {
                    TerminalParser::CsiIgnored
                }
            }
            TerminalParser::Osc { esc_seen } => match (esc_seen, ch) {
                (_, '\u{7}') => TerminalParser::Ground,
                (true, '\\') => TerminalParser::Ground,
                (_, '\u{1b}') => TerminalParser::Osc { esc_seen: true },
                _ => TerminalParser::Osc { esc_seen: false },
            },
            TerminalParser::IgnoreOne => TerminalParser::Ground,
        };
    }

    fn process_ground_char(&mut self, ch: char) -> TerminalParser {
        match ch {
            '\u{1b}' => TerminalParser::Escape,
            '\n' => {
                self.screen.line_feed();
                TerminalParser::Ground
            }
            '\r' => {
                self.screen.carriage_return();
                TerminalParser::Ground
            }
            '\t' => {
                self.screen.tab();
                TerminalParser::Ground
            }
            '\u{8}' => {
                self.screen.backspace();
                TerminalParser::Ground
            }
            ch if ch.is_control() => TerminalParser::Ground,
            ch => {
                self.screen.put_char(ch);
                TerminalParser::Ground
            }
        }
    }

    fn process_escape_char(&mut self, ch: char, responses: &mut Vec<Vec<u8>>) -> TerminalParser {
        match ch {
            '[' => TerminalParser::Csi(String::new()),
            ']' => TerminalParser::Osc { esc_seen: false },
            '(' | ')' | '*' | '+' => TerminalParser::IgnoreOne,
            'c' => {
                self.screen.reset();
                TerminalParser::Ground
            }
            'Z' => {
                responses.push(PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.to_vec());
                TerminalParser::Ground
            }
            '7' => {
                self.screen.save_cursor();
                TerminalParser::Ground
            }
            '8' => {
                self.screen.restore_cursor();
                TerminalParser::Ground
            }
            'D' => {
                self.screen.line_feed();
                TerminalParser::Ground
            }
            'E' => {
                self.screen.carriage_return();
                self.screen.line_feed();
                TerminalParser::Ground
            }
            'M' => {
                self.screen.reverse_index();
                TerminalParser::Ground
            }
            _ => TerminalParser::Ground,
        }
    }

    fn apply_csi(&mut self, sequence: &str, final_char: char, responses: &mut Vec<Vec<u8>>) {
        let private = sequence.contains('?');
        let params = parse_csi_params(sequence);
        match final_char {
            'A' => self.screen.move_cursor_up(param_or_default(&params, 0, 1)),
            'B' => self
                .screen
                .move_cursor_down(param_or_default(&params, 0, 1)),
            'C' => self
                .screen
                .move_cursor_right(param_or_default(&params, 0, 1)),
            'D' => self
                .screen
                .move_cursor_left(param_or_default(&params, 0, 1)),
            'G' => self.screen.set_cursor_col(param_or_default(&params, 0, 1)),
            'H' | 'f' => self.screen.set_cursor_position(
                param_or_default(&params, 0, 1),
                param_or_default(&params, 1, 1),
            ),
            'J' => self.screen.erase_display(param_or_default(&params, 0, 0)),
            'K' => self.screen.erase_line(param_or_default(&params, 0, 0)),
            'c' if !private && param_or_default(&params, 0, 0) == 0 => {
                responses.push(PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.to_vec());
            }
            'm' => self.screen.apply_sgr(&params),
            'n' if !private => match param_or_default(&params, 0, 0) {
                5 => responses.push(DEVICE_STATUS_OK_RESPONSE.to_vec()),
                6 => responses.push(self.screen.cursor_position_report(false)),
                _ => {}
            },
            'n' if private && param_or_default(&params, 0, 0) == 6 => {
                responses.push(self.screen.cursor_position_report(true));
            }
            'r' => self.screen.set_scroll_region_from_params(&params),
            'S' => self.screen.scroll_up(param_or_default(&params, 0, 1)),
            'T' => self.screen.scroll_down(param_or_default(&params, 0, 1)),
            'd' => self.screen.set_cursor_row(param_or_default(&params, 0, 1)),
            's' => self.screen.save_cursor(),
            'u' => self.screen.restore_cursor(),
            'h' if private => self.screen.set_private_modes(&params, true),
            'l' if private => self.screen.set_private_modes(&params, false),
            _ => {}
        }
    }
}

impl Default for TerminalScreen {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

impl TerminalScreen {
    fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = bounded_terminal_dimensions(rows, cols);
        Self {
            rows,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            wrap_pending: false,
            saved_cursor: None,
            current_style: TerminalCellStyle::default(),
            application_cursor_keys: false,
            bracketed_paste: false,
            scroll_top: 0,
            scroll_bottom: usize::from(rows).saturating_sub(1),
            scrollback: VecDeque::new(),
            cells: vec![vec![TerminalCell::blank(); usize::from(cols)]; usize::from(rows)],
            alternate_saved: None,
        }
    }

    fn resize(&mut self, rows: u16, cols: u16, preserve_bottom: bool) {
        let old_scroll_region_was_full_screen =
            self.scroll_top == 0 && self.scroll_bottom + 1 == self.cells.len();
        let (rows, cols) = bounded_terminal_dimensions(rows, cols);
        self.rows = rows;
        self.cols = cols;
        let row_len = usize::from(cols);
        let row_count = usize::from(rows);

        for row in &mut self.scrollback {
            row.resize(row_len, TerminalCell::blank());
        }
        for row in &mut self.cells {
            row.resize(row_len, TerminalCell::blank());
        }

        if preserve_bottom && old_scroll_region_was_full_screen {
            self.resize_rows_preserving_bottom(row_count);
        } else {
            self.cells.resize(row_count, blank_terminal_row(row_len));
        }

        if old_scroll_region_was_full_screen {
            self.reset_scroll_region();
        } else {
            self.clamp_scroll_region();
        }
        self.clamp_cursor();
    }

    fn resize_rows_preserving_bottom(&mut self, row_count: usize) {
        let old_row_count = self.cells.len();
        match row_count.cmp(&old_row_count) {
            std::cmp::Ordering::Greater => {
                let added_rows = row_count - old_row_count;
                let pulled_scrollback_rows = added_rows.min(self.scrollback.len());
                let blank_rows = added_rows - pulled_scrollback_rows;
                let mut cells = Vec::with_capacity(row_count);
                cells.extend(
                    std::iter::repeat_with(|| blank_terminal_row(usize::from(self.cols)))
                        .take(blank_rows),
                );
                if pulled_scrollback_rows > 0 {
                    let split_at = self.scrollback.len() - pulled_scrollback_rows;
                    cells.extend(self.scrollback.drain(split_at..));
                }
                cells.append(&mut self.cells);
                self.cells = cells;
                self.cursor_row = self
                    .cursor_row
                    .saturating_add(added_rows)
                    .min(row_count.saturating_sub(1));
            }
            std::cmp::Ordering::Less => {
                let removed_rows = old_row_count - row_count;
                let removed = self.cells.drain(..removed_rows).collect::<Vec<_>>();
                for row in removed {
                    if !terminal_row_is_blank(&row) {
                        self.push_scrollback(row);
                    }
                }
                self.cursor_row = self
                    .cursor_row
                    .saturating_sub(removed_rows)
                    .min(row_count.saturating_sub(1));
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    fn visible_lines(&self, scroll_offset: usize) -> Vec<String> {
        self.viewport_row_indices(scroll_offset)
            .filter_map(|row_index| self.row_at(row_index))
            .map(terminal_row_text)
            .collect()
    }

    fn all_lines(&self) -> Vec<String> {
        self.scrollback
            .iter()
            .chain(self.cells.iter())
            .map(|row| terminal_row_text(row))
            .collect()
    }

    fn render_lines(&self, scroll_offset: usize) -> Vec<TerminalRenderLine> {
        self.render_lines_with_forced_cursor(scroll_offset, false)
    }

    fn render_lines_with_forced_cursor(
        &self,
        scroll_offset: usize,
        force_cursor_visible: bool,
    ) -> Vec<TerminalRenderLine> {
        let scroll_offset = scroll_offset.min(self.max_scroll_offset());
        let cursor_visible = self.cursor_visible || force_cursor_visible;
        let cursor_row = (scroll_offset == 0 && cursor_visible)
            .then_some(self.scrollback.len() + self.cursor_row);

        self.viewport_row_indices(scroll_offset)
            .filter_map(|row_index| {
                let row = self.row_at(row_index)?;
                let cursor_col = (cursor_row == Some(row_index)).then_some(
                    self.cursor_col
                        .min(usize::from(self.cols).saturating_sub(1)),
                );
                Some(render_terminal_row(row, cursor_col))
            })
            .collect()
    }

    fn snapshot(&self, scroll_offset: usize) -> ScreenSnapshot {
        let scroll_offset = scroll_offset.min(self.max_scroll_offset());
        let cursor = (scroll_offset == 0).then_some(Cursor {
            row: self.cursor_row as u16,
            col: self
                .cursor_col
                .min(usize::from(self.cols).saturating_sub(1)) as u16,
            visible: self.cursor_visible,
        });
        let mut cells = Vec::with_capacity(usize::from(self.rows) * usize::from(self.cols));
        for row_index in self.viewport_row_indices(scroll_offset) {
            if let Some(row) = self.row_at(row_index) {
                cells.extend_from_slice(row);
            } else {
                cells.extend(std::iter::repeat_n(
                    TerminalCell::blank(),
                    usize::from(self.cols),
                ));
            }
        }

        ScreenSnapshot {
            rows: self.rows,
            cols: self.cols,
            cells,
            cursor,
            scrollback_rows: self.scrollback.len() as u32,
        }
    }

    fn is_blank(&self) -> bool {
        self.scrollback
            .iter()
            .chain(self.cells.iter())
            .all(|row| row.iter().all(|cell| cell.ch == ' '))
    }

    fn max_scroll_offset(&self) -> usize {
        self.scrollback.len()
    }

    fn viewport_row_indices(&self, scroll_offset: usize) -> std::ops::Range<usize> {
        let rows = usize::from(self.rows);
        let total_rows = self.scrollback.len() + self.cells.len();
        let scroll_offset = scroll_offset.min(self.max_scroll_offset());
        let end = total_rows.saturating_sub(scroll_offset);
        end.saturating_sub(rows)..end
    }

    fn row_at(&self, row_index: usize) -> Option<&[TerminalCell]> {
        if row_index < self.scrollback.len() {
            self.scrollback.get(row_index).map(Vec::as_slice)
        } else {
            self.cells
                .get(row_index.saturating_sub(self.scrollback.len()))
                .map(Vec::as_slice)
        }
    }

    fn put_char(&mut self, ch: char) {
        if self.wrap_pending {
            self.carriage_return();
            self.line_feed();
        }

        self.cells[self.cursor_row][self.cursor_col] = TerminalCell {
            ch,
            style: self.current_style,
        };

        let last_col = usize::from(self.cols).saturating_sub(1);
        if self.cursor_col >= last_col {
            self.wrap_pending = true;
        } else {
            self.cursor_col += 1;
            self.wrap_pending = false;
        }
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor_row + 1 < usize::from(self.rows) {
            self.cursor_row += 1;
        }
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
        self.wrap_pending = false;
    }

    fn tab(&mut self) {
        self.wrap_pending = false;
        let next_tab = ((self.cursor_col / 8) + 1) * 8;
        self.cursor_col = next_tab.min(usize::from(self.cols).saturating_sub(1));
    }

    fn backspace(&mut self) {
        self.wrap_pending = false;
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_top {
            self.scroll_down(1);
        } else {
            self.cursor_row = self.cursor_row.saturating_sub(1);
        }
    }

    fn move_cursor_up(&mut self, count: usize) {
        self.wrap_pending = false;
        self.cursor_row = self.cursor_row.saturating_sub(count);
    }

    fn move_cursor_down(&mut self, count: usize) {
        self.wrap_pending = false;
        self.cursor_row = self
            .cursor_row
            .saturating_add(count)
            .min(usize::from(self.rows).saturating_sub(1));
    }

    fn move_cursor_right(&mut self, count: usize) {
        self.wrap_pending = false;
        self.cursor_col = self
            .cursor_col
            .saturating_add(count)
            .min(usize::from(self.cols).saturating_sub(1));
    }

    fn move_cursor_left(&mut self, count: usize) {
        self.wrap_pending = false;
        self.cursor_col = self.cursor_col.saturating_sub(count);
    }

    fn set_cursor_position(&mut self, row: usize, col: usize) {
        self.wrap_pending = false;
        self.cursor_row = row
            .saturating_sub(1)
            .min(usize::from(self.rows).saturating_sub(1));
        self.cursor_col = col
            .saturating_sub(1)
            .min(usize::from(self.cols).saturating_sub(1));
    }

    fn set_cursor_row(&mut self, row: usize) {
        self.wrap_pending = false;
        self.cursor_row = row
            .saturating_sub(1)
            .min(usize::from(self.rows).saturating_sub(1));
    }

    fn set_cursor_col(&mut self, col: usize) {
        self.wrap_pending = false;
        self.cursor_col = col
            .saturating_sub(1)
            .min(usize::from(self.cols).saturating_sub(1));
    }

    fn erase_display(&mut self, mode: usize) {
        self.wrap_pending = false;
        match mode {
            0 => {
                self.erase_line_from_cursor();
                for row in self.cursor_row + 1..usize::from(self.rows) {
                    self.clear_row(row);
                }
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.clear_row(row);
                }
                self.erase_line_to_cursor();
            }
            2 => self.clear(),
            3 => self.clear_scrollback(),
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        self.wrap_pending = false;
        match mode {
            0 => self.erase_line_from_cursor(),
            1 => self.erase_line_to_cursor(),
            2 => self.clear_row(self.cursor_row),
            _ => {}
        }
    }

    fn scroll_up(&mut self, count: usize) {
        for _ in 0..self.scroll_region_count(count) {
            if self.cells.is_empty() {
                return;
            }
            let row = self.cells.remove(self.scroll_top);
            if self.scroll_top == 0 {
                self.push_scrollback(row);
            }
            self.cells.insert(
                self.scroll_bottom,
                vec![TerminalCell::blank(); usize::from(self.cols)],
            );
        }
    }

    fn scroll_down(&mut self, count: usize) {
        for _ in 0..self.scroll_region_count(count) {
            if self.cells.is_empty() {
                return;
            }
            self.cells.remove(self.scroll_bottom);
            self.cells.insert(
                self.scroll_top,
                vec![TerminalCell::blank(); usize::from(self.cols)],
            );
        }
    }

    fn scroll_region_count(&self, count: usize) -> usize {
        let region_rows = self.scroll_bottom.saturating_sub(self.scroll_top) + 1;
        count.max(1).min(region_rows)
    }

    fn set_scroll_region_from_params(&mut self, params: &[usize]) {
        if params.is_empty() {
            self.reset_scroll_region();
            return;
        }

        let top = param_or_default(params, 0, 1).saturating_sub(1);
        let bottom = param_or_default(params, 1, usize::from(self.rows)).saturating_sub(1);
        if top < bottom && bottom < usize::from(self.rows) {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
            self.set_cursor_position(1, 1);
        }
    }

    fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = usize::from(self.rows).saturating_sub(1);
    }

    fn clamp_scroll_region(&mut self) {
        let last_row = usize::from(self.rows).saturating_sub(1);
        self.scroll_top = self.scroll_top.min(last_row);
        self.scroll_bottom = self.scroll_bottom.min(last_row);
        if self.scroll_top >= self.scroll_bottom {
            self.reset_scroll_region();
        }
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_row, self.cursor_col));
    }

    fn restore_cursor(&mut self) {
        if let Some((row, col)) = self.saved_cursor {
            self.cursor_row = row;
            self.cursor_col = col;
            self.wrap_pending = false;
            self.clamp_cursor();
        }
    }

    fn clear(&mut self) {
        for row in 0..usize::from(self.rows) {
            self.clear_row(row);
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.wrap_pending = false;
    }

    fn reset(&mut self) {
        self.clear();
        self.clear_scrollback();
        self.reset_scroll_region();
        self.current_style = TerminalCellStyle::default();
        self.cursor_visible = true;
        self.application_cursor_keys = false;
        self.bracketed_paste = false;
        self.saved_cursor = None;
        self.alternate_saved = None;
    }

    fn cursor_position_report(&self, private: bool) -> Vec<u8> {
        if private {
            format!("\x1b[?{};{}R", self.cursor_row + 1, self.cursor_col + 1).into_bytes()
        } else {
            format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_col + 1).into_bytes()
        }
    }

    fn set_private_modes(&mut self, params: &[usize], enabled: bool) {
        for param in params {
            match *param {
                1 => self.application_cursor_keys = enabled,
                25 => self.cursor_visible = enabled,
                47 | 1047 | 1049 if enabled => self.enter_alternate_screen(),
                47 | 1047 | 1049 => self.leave_alternate_screen(),
                2004 => self.bracketed_paste = enabled,
                _ => {}
            }
        }
    }

    fn enter_alternate_screen(&mut self) {
        if self.alternate_saved.is_none() {
            self.alternate_saved = Some(self.save_screen_state());
        }
        self.clear();
        self.clear_scrollback();
        self.reset_scroll_region();
    }

    fn leave_alternate_screen(&mut self) {
        if let Some(state) = self.alternate_saved.take() {
            self.restore_screen_state(state);
        } else {
            self.clear();
            self.clear_scrollback();
            self.reset_scroll_region();
        }
    }

    fn save_screen_state(&self) -> TerminalScreenState {
        TerminalScreenState {
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            cursor_visible: self.cursor_visible,
            wrap_pending: self.wrap_pending,
            saved_cursor: self.saved_cursor,
            current_style: self.current_style,
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
            scrollback: self.scrollback.clone(),
            cells: self.cells.clone(),
        }
    }

    fn restore_screen_state(&mut self, state: TerminalScreenState) {
        self.cursor_row = state.cursor_row;
        self.cursor_col = state.cursor_col;
        self.cursor_visible = state.cursor_visible;
        self.wrap_pending = state.wrap_pending;
        self.saved_cursor = state.saved_cursor;
        self.current_style = state.current_style;
        self.scroll_top = state.scroll_top;
        self.scroll_bottom = state.scroll_bottom;
        self.scrollback = state.scrollback;
        self.cells = state.cells;
        self.resize(self.rows, self.cols, true);
    }

    fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    fn push_scrollback(&mut self, row: Vec<TerminalCell>) {
        self.scrollback.push_back(row);
        let overflow = self
            .scrollback
            .len()
            .saturating_sub(TERMINAL_MAX_SCROLLBACK_LINES);
        for _ in 0..overflow {
            self.scrollback.pop_front();
        }
    }

    fn erase_line_from_cursor(&mut self) {
        let cols = usize::from(self.cols);
        for col in self.cursor_col..cols {
            self.cells[self.cursor_row][col] = TerminalCell::blank();
        }
    }

    fn erase_line_to_cursor(&mut self) {
        let end = self
            .cursor_col
            .min(usize::from(self.cols).saturating_sub(1));
        for col in 0..=end {
            self.cells[self.cursor_row][col] = TerminalCell::blank();
        }
    }

    fn clear_row(&mut self, row: usize) {
        if let Some(row) = self.cells.get_mut(row) {
            row.fill(TerminalCell::blank());
        }
    }

    fn apply_sgr(&mut self, params: &[usize]) {
        if params.is_empty() {
            self.current_style = TerminalCellStyle::default();
            return;
        }

        let mut index = 0;
        while index < params.len() {
            match params[index] {
                0 => self.current_style = TerminalCellStyle::default(),
                1 => self.current_style.bold = true,
                3 => self.current_style.italic = true,
                4 => self.current_style.underlined = true,
                7 => self.current_style.reversed = true,
                22 => self.current_style.bold = false,
                23 => self.current_style.italic = false,
                24 => self.current_style.underlined = false,
                27 => self.current_style.reversed = false,
                30..=37 => self.current_style.fg = ansi_color(params[index] - 30, false),
                39 => self.current_style.fg = None,
                40..=47 => self.current_style.bg = ansi_color(params[index] - 40, false),
                49 => self.current_style.bg = None,
                90..=97 => self.current_style.fg = ansi_color(params[index] - 90, true),
                100..=107 => self.current_style.bg = ansi_color(params[index] - 100, true),
                38 | 48 => {
                    let is_fg = params[index] == 38;
                    if let Some((color, consumed)) = extended_color(&params[index + 1..]) {
                        if is_fg {
                            self.current_style.fg = Some(color);
                        } else {
                            self.current_style.bg = Some(color);
                        }
                        index += consumed;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }

    fn clamp_cursor(&mut self) {
        self.cursor_row = self
            .cursor_row
            .min(usize::from(self.rows).saturating_sub(1));
        self.cursor_col = self
            .cursor_col
            .min(usize::from(self.cols).saturating_sub(1));
        self.wrap_pending = false;
    }
}

fn bounded_terminal_dimensions(rows: u16, cols: u16) -> (u16, u16) {
    bounded_screen_dimensions(rows.max(1), cols.max(1))
}

fn blank_terminal_row(cols: usize) -> Vec<TerminalCell> {
    vec![TerminalCell::blank(); cols]
}

fn terminal_row_is_blank(row: &[TerminalCell]) -> bool {
    row.iter().all(|cell| *cell == TerminalCell::blank())
}

fn terminal_row_text(row: &[TerminalCell]) -> String {
    row.iter()
        .map(|cell| cell.ch)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn render_terminal_row(row: &[TerminalCell], cursor_col: Option<usize>) -> TerminalRenderLine {
    let last_visible_cell = row
        .iter()
        .rposition(|cell| cell.ch != ' ' || cell.style != TerminalCellStyle::default());
    let last_visible = last_visible_cell.into_iter().chain(cursor_col).max();
    let Some(last_visible) = last_visible else {
        return TerminalRenderLine { spans: Vec::new() };
    };

    let mut spans = Vec::new();
    let mut current_style = row[0].style;
    let mut text = String::new();
    for (index, cell) in row[..=last_visible].iter().enumerate() {
        let mut cell = *cell;
        if cursor_col == Some(index) {
            cell.style = cursor_style(cell.style);
            if cell.ch == ' ' {
                cell.ch = '▌';
            }
        }
        if cell.style != current_style && !text.is_empty() {
            spans.push(TerminalRenderSpan {
                text: std::mem::take(&mut text),
                style: current_style,
            });
        }
        current_style = cell.style;
        text.push(cell.ch);
    }
    if !text.is_empty() {
        spans.push(TerminalRenderSpan {
            text,
            style: current_style,
        });
    }

    TerminalRenderLine { spans }
}

fn cursor_style(mut style: TerminalCellStyle) -> TerminalCellStyle {
    style.fg = Some(TerminalColor::Black);
    style.bg = Some(TerminalColor::BrightWhite);
    style.underlined = false;
    style.reversed = false;
    style
}

fn terminal_paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }

    let mut bytes =
        Vec::with_capacity(BRACKETED_PASTE_START.len() + text.len() + BRACKETED_PASTE_END.len());
    bytes.extend_from_slice(BRACKETED_PASTE_START);
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(BRACKETED_PASTE_END);
    bytes
}

fn ansi_color(index: usize, bright: bool) -> Option<TerminalColor> {
    Some(match (index, bright) {
        (0, false) => TerminalColor::Black,
        (1, false) => TerminalColor::Red,
        (2, false) => TerminalColor::Green,
        (3, false) => TerminalColor::Yellow,
        (4, false) => TerminalColor::Blue,
        (5, false) => TerminalColor::Magenta,
        (6, false) => TerminalColor::Cyan,
        (7, false) => TerminalColor::White,
        (0, true) => TerminalColor::BrightBlack,
        (1, true) => TerminalColor::BrightRed,
        (2, true) => TerminalColor::BrightGreen,
        (3, true) => TerminalColor::BrightYellow,
        (4, true) => TerminalColor::BrightBlue,
        (5, true) => TerminalColor::BrightMagenta,
        (6, true) => TerminalColor::BrightCyan,
        (7, true) => TerminalColor::BrightWhite,
        _ => return None,
    })
}

fn extended_color(params: &[usize]) -> Option<(TerminalColor, usize)> {
    match params {
        [2, red @ 0..=255, green @ 0..=255, blue @ 0..=255, ..] => {
            Some((TerminalColor::Rgb(*red as u8, *green as u8, *blue as u8), 4))
        }
        [5, index @ 0..=255, ..] => Some((xterm_256_color(*index), 2)),
        _ => None,
    }
}

fn xterm_256_color(index: usize) -> TerminalColor {
    if index < 8 {
        ansi_color(index, false).unwrap_or(TerminalColor::White)
    } else if index < 16 {
        ansi_color(index - 8, true).unwrap_or(TerminalColor::BrightWhite)
    } else if (16..=231).contains(&index) {
        let value = index - 16;
        let red = value / 36;
        let green = (value / 6) % 6;
        let blue = value % 6;
        TerminalColor::Rgb(
            color_cube_value(red),
            color_cube_value(green),
            color_cube_value(blue),
        )
    } else {
        let gray = 8 + ((index.saturating_sub(232)) * 10).min(238);
        TerminalColor::Rgb(gray as u8, gray as u8, gray as u8)
    }
}

fn color_cube_value(value: usize) -> u8 {
    if value == 0 {
        0
    } else {
        (55 + value * 40) as u8
    }
}

fn parse_csi_params(sequence: &str) -> Vec<usize> {
    sequence
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

fn param_or_default(params: &[usize], index: usize, default: usize) -> usize {
    params
        .get(index)
        .copied()
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn screen_snapshot_lines(snapshot: &ScreenSnapshot) -> Vec<String> {
    snapshot
        .render_lines()
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.text)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
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

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
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

    fn rendered_text(lines: Vec<TerminalRenderLine>) -> String {
        lines
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|span| span.text)
            .collect()
    }

    #[test]
    fn navigation_contains_nested_workspace_items_with_stable_ids() {
        let app = App::default();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;

        assert_eq!(app.nav_len(), app.nav_items().len());
        assert_eq!(app.nav_item_at(0), Some(NavItem::Workspace(workspace)));
        assert_eq!(app.nav_item_at(1), Some(NavItem::Chat { workspace, chat }));
        assert_eq!(app.nav_item_at(app.nav_len()), None);
        assert!(app
            .nav_items()
            .iter()
            .any(|item| matches!(item, NavItem::Terminal { .. })));
    }

    #[test]
    fn focus_modes_cycle_between_sidebar_and_selected_main_pane() {
        let mut app = App {
            selected: 1,
            ..App::default()
        };
        assert!(matches!(app.selected_item(), Some(NavItem::Chat { .. })));

        assert_eq!(app.focus, FocusMode::Sidebar);
        app.focus_next();
        assert_eq!(app.focus, FocusMode::Chat);
        app.focus_next();
        assert_eq!(app.focus, FocusMode::Sidebar);

        let terminal_index = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Terminal { .. }))
            .expect("seed state has a terminal");
        app.selected = terminal_index;
        app.focus_next();
        assert_eq!(app.focus, FocusMode::Terminal);
        app.focus_previous();
        assert_eq!(app.focus, FocusMode::Sidebar);
    }

    #[test]
    fn focus_falls_back_to_sidebar_for_workspace_selection() {
        let mut app = App {
            selected: 1,
            focus: FocusMode::Chat,
            ..App::default()
        };

        app.select_previous();

        assert!(matches!(app.selected_item(), Some(NavItem::Workspace(_))));
        assert_eq!(app.focus, FocusMode::Sidebar);
        assert!(!app.focus_selected_main());
    }

    #[test]
    fn adding_chat_can_return_workspace_and_chat_ids_for_auto_start() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;

        let added = app
            .add_chat_to_selected_workspace_and_return()
            .expect("chat is added");

        let chat = app.project.workspaces[0].chats.last().unwrap().id;
        assert_eq!(added, (workspace, chat));
        assert_eq!(app.selected_item(), Some(NavItem::Chat { workspace, chat }));
    }

    #[test]
    fn selected_terminal_can_enter_input_mode() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });

        assert!(app.begin_terminal_input());

        assert_eq!(app.terminal_input_target(), Some(terminal));
        assert_eq!(app.pty_input_target(), Some(terminal));
        assert_eq!(
            app.mode,
            Mode::Input(InputTarget::Terminal {
                workspace,
                terminal,
            })
        );
        assert_eq!(app.focus, FocusMode::Terminal);
    }

    #[test]
    fn selected_chat_can_enter_pi_agent_input_mode() {
        let mut app = App {
            selected: 1,
            ..App::default()
        };
        let Some(NavItem::Chat { workspace, chat }) = app.selected_item() else {
            panic!("expected selected chat");
        };

        assert!(app.begin_chat_agent_input());

        assert_eq!(app.pty_input_target(), Some(chat_agent_terminal_id(chat)));
        assert_eq!(
            chat_id_from_agent_terminal_id(chat_agent_terminal_id(chat)),
            Some(chat)
        );
        assert_eq!(
            app.mode,
            Mode::Input(InputTarget::ChatAgent { workspace, chat })
        );
        assert_eq!(app.focus, FocusMode::Chat);
    }

    #[test]
    fn ending_input_mode_returns_focus_to_sidebar() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        assert!(app.begin_terminal_input());

        app.end_pty_input();

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.focus, FocusMode::Sidebar);
    }

    #[test]
    fn terminal_buffer_handles_cursor_positioning_and_osc_links() {
        let mut app = App::default();
        let terminal = TerminalId(99);
        app.resize_terminal_buffer(terminal, 3, 12);

        app.append_terminal_output(
            terminal,
            "\x1b[2J\x1b[2;3Hhi \x1b]8;;https://example.com\x07link\x1b]8;;\x07",
        );

        let lines = app.terminal_lines(terminal);
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "  hi link");
        assert_eq!(lines[2], "");
    }

    #[test]
    fn terminal_buffer_clears_and_rewrites_screen() {
        let mut app = App::default();
        let terminal = TerminalId(100);
        app.resize_terminal_buffer(terminal, 2, 8);

        app.append_terminal_output(terminal, "old\x1b[2J\x1b[1;1Hnew");

        assert_eq!(
            app.terminal_lines(terminal),
            vec!["new".to_string(), "".to_string()]
        );
    }

    #[test]
    fn terminal_buffer_wraps_printable_text_at_right_edge() {
        let mut app = App::default();
        let terminal = TerminalId(107);
        app.resize_terminal_buffer(terminal, 2, 3);

        app.append_terminal_output(terminal, "abcd");

        assert_eq!(
            app.terminal_lines(terminal),
            vec!["abc".to_string(), "d".to_string()]
        );
    }

    #[test]
    fn terminal_buffer_saturates_large_cursor_moves() {
        let mut app = App::default();
        let terminal = TerminalId(110);
        app.resize_terminal_buffer(terminal, 1, 4);
        let huge = usize::MAX.to_string();

        app.append_terminal_output(terminal, &format!("a\x1b[{huge}Cz"));

        assert_eq!(app.terminal_lines(terminal), vec!["a  z".to_string()]);
    }

    #[test]
    fn terminal_buffer_clamps_large_scroll_counts_to_visible_rows() {
        let mut app = App::default();
        let terminal = TerminalId(111);
        app.resize_terminal_buffer(terminal, 2, 4);

        app.append_terminal_output(terminal, "a\r\nb\x1b[999999S");

        assert_eq!(
            app.terminal_lines(terminal),
            vec!["".to_string(), "".to_string()]
        );
        assert_eq!(
            app.terminal_buffers
                .get(&terminal)
                .expect("terminal buffer exists")
                .screen
                .scrollback
                .len(),
            2
        );
    }

    #[test]
    fn terminal_buffer_resize_clamps_huge_dimensions() {
        let mut app = App::default();
        let terminal = TerminalId(112);

        app.resize_terminal_buffer(terminal, u16::MAX, u16::MAX);

        let screen = &app
            .terminal_buffers
            .get(&terminal)
            .expect("terminal buffer exists")
            .screen;
        assert!(
            usize::from(screen.rows) * usize::from(screen.cols) <= mult_protocol::MAX_SCREEN_CELLS
        );
        assert_eq!(screen.cells.len(), usize::from(screen.rows));
        assert!(screen
            .cells
            .iter()
            .all(|row| row.len() == usize::from(screen.cols)));
    }

    #[test]
    fn terminal_buffer_resize_grow_keeps_bottom_view_anchored() {
        let mut app = App::default();
        let terminal = TerminalId(120);
        app.resize_terminal_buffer(terminal, 2, 8);
        app.append_terminal_output(terminal, "one\r\ntwo\r\nthree");

        app.resize_terminal_buffer(terminal, 4, 8);

        assert_eq!(
            app.terminal_lines(terminal),
            vec![
                "".to_string(),
                "one".to_string(),
                "two".to_string(),
                "three".to_string(),
            ]
        );
        let screen = &app
            .terminal_buffers
            .get(&terminal)
            .expect("terminal buffer exists")
            .screen;
        assert_eq!(screen.cursor_row, 3);
        assert_eq!(screen.scroll_bottom, 3);
    }

    #[test]
    fn terminal_buffer_resize_shrink_ignores_discarded_blank_rows() {
        let mut app = App::default();
        let terminal = TerminalId(121);

        app.resize_terminal_buffer(terminal, 4, 8);
        app.resize_terminal_buffer(terminal, 2, 8);

        assert!(!app.scroll_terminal_output_up(terminal, 1));
    }

    #[test]
    fn terminal_buffer_scrolls_back_and_returns_to_bottom() {
        let mut app = App::default();
        let terminal = TerminalId(104);
        app.resize_terminal_buffer(terminal, 2, 8);

        app.append_terminal_output(terminal, "one\r\ntwo\r\nthree");

        assert_eq!(
            app.terminal_lines(terminal),
            vec!["two".to_string(), "three".to_string()]
        );
        assert!(app.scroll_terminal_output_up(terminal, 1));
        assert_eq!(
            app.terminal_lines(terminal),
            vec!["one".to_string(), "two".to_string()]
        );
        assert!(!app.scroll_terminal_output_up(terminal, 1));
        assert!(app.scroll_terminal_output_down(terminal, 1));
        assert_eq!(
            app.terminal_lines(terminal),
            vec!["two".to_string(), "three".to_string()]
        );
    }

    #[test]
    fn terminal_buffer_preserves_scrolled_view_when_output_arrives() {
        let mut app = App::default();
        let terminal = TerminalId(105);
        app.resize_terminal_buffer(terminal, 2, 8);
        app.append_terminal_output(terminal, "one\r\ntwo\r\nthree");
        app.scroll_terminal_output_up(terminal, 1);

        app.append_terminal_output(terminal, "\r\nfour");

        assert_eq!(
            app.terminal_lines(terminal),
            vec!["one".to_string(), "two".to_string()]
        );
        app.scroll_terminal_output_to_bottom(terminal);
        assert_eq!(
            app.terminal_lines(terminal),
            vec!["three".to_string(), "four".to_string()]
        );
    }

    #[test]
    fn terminal_output_blank_check_includes_scrollback() {
        let mut app = App::default();
        let terminal = TerminalId(106);
        app.resize_terminal_buffer(terminal, 1, 8);

        app.append_terminal_output(terminal, "old\r\n");

        assert_eq!(app.terminal_lines(terminal), vec!["".to_string()]);
        assert!(!app.terminal_output_is_blank(terminal));
    }

    #[test]
    fn terminal_buffer_preserves_sgr_colors() {
        let mut app = App::default();
        let terminal = TerminalId(101);
        app.resize_terminal_buffer(terminal, 1, 16);

        app.append_terminal_output(terminal, "plain \x1b[31;1mred\x1b[0m ok");

        let lines = app.terminal_render_lines(terminal);
        assert_eq!(lines[0].spans[0].text, "plain ");
        assert_eq!(lines[0].spans[1].text, "red");
        assert_eq!(lines[0].spans[1].style.fg, Some(TerminalColor::Red));
        assert!(lines[0].spans[1].style.bold);
        assert_eq!(lines[0].spans[2].text, " ok");
        assert_eq!(lines[0].spans[2].style, TerminalCellStyle::default());
    }

    #[test]
    fn terminal_buffer_ignores_invalid_extended_sgr_colors() {
        let mut app = App::default();
        let terminal = TerminalId(108);
        app.resize_terminal_buffer(terminal, 1, 16);

        app.append_terminal_output(terminal, "\x1b[38;2;999;0;0mbad");

        let lines = app.terminal_render_lines(terminal);
        assert_eq!(lines[0].spans[0].text, "bad");
        assert_eq!(lines[0].spans[0].style, TerminalCellStyle::default());
    }

    #[test]
    fn terminal_buffer_ignores_oversized_csi_sequences() {
        let mut app = App::default();
        let terminal = TerminalId(109);
        app.resize_terminal_buffer(terminal, 1, 16);
        let long_params = "1".repeat(TERMINAL_MAX_CSI_SEQUENCE_CHARS + 1);

        app.append_terminal_output(terminal, &format!("\x1b[{long_params}mOK"));

        assert_eq!(app.terminal_lines(terminal), vec!["OK".to_string()]);
    }

    #[test]
    fn terminal_buffer_handles_tabs() {
        let mut app = App::default();
        let terminal = TerminalId(113);
        app.resize_terminal_buffer(terminal, 1, 12);

        app.append_terminal_output(terminal, "a\tb");

        assert_eq!(app.terminal_lines(terminal), vec!["a       b".to_string()]);
    }

    #[test]
    fn terminal_buffer_saves_and_restores_cursor() {
        let mut app = App::default();
        let terminal = TerminalId(114);
        app.resize_terminal_buffer(terminal, 1, 8);

        app.append_terminal_output(terminal, "ab\x1b7\x1b[1;5Hxy\x1b8Z");

        assert_eq!(app.terminal_lines(terminal), vec!["abZ xy".to_string()]);
    }

    #[test]
    fn terminal_buffer_applies_erase_modes() {
        let mut app = App::default();
        let terminal = TerminalId(115);
        app.resize_terminal_buffer(terminal, 2, 8);

        app.append_terminal_output(
            terminal,
            "abcdef\x1b[1;3H\x1b[K\x1b[2;1Hzzzz\x1b[2;3H\x1b[1K",
        );

        assert_eq!(
            app.terminal_lines(terminal),
            vec!["ab".to_string(), "   z".to_string()]
        );
    }

    #[test]
    fn terminal_buffer_resets_sgr_attributes_selectively() {
        let mut app = App::default();
        let terminal = TerminalId(116);
        app.resize_terminal_buffer(terminal, 1, 16);

        app.append_terminal_output(terminal, "\x1b[31;1mred\x1b[22;39mplain");

        let lines = app.terminal_render_lines(terminal);
        assert_eq!(lines[0].spans[0].text, "red");
        assert_eq!(lines[0].spans[0].style.fg, Some(TerminalColor::Red));
        assert!(lines[0].spans[0].style.bold);
        assert_eq!(lines[0].spans[1].text, "plain");
        assert_eq!(lines[0].spans[1].style, TerminalCellStyle::default());
    }

    #[test]
    fn terminal_buffer_respects_scroll_regions() {
        let mut app = App::default();
        let terminal = TerminalId(117);
        app.resize_terminal_buffer(terminal, 4, 8);

        app.append_terminal_output(
            terminal,
            "one\r\ntwo\r\nthree\r\nfour\x1b[2;3r\x1b[3;1HXX\r\nYY",
        );

        assert_eq!(
            app.terminal_lines(terminal),
            vec![
                "one".to_string(),
                "XXree".to_string(),
                "YY".to_string(),
                "four".to_string()
            ]
        );
    }

    #[test]
    fn terminal_buffer_restores_main_screen_after_alternate_screen() {
        let mut app = App::default();
        let terminal = TerminalId(118);
        app.resize_terminal_buffer(terminal, 2, 8);

        app.append_terminal_output(terminal, "main\r\nkeep\x1b[?1049halt\x1b[?1049l");

        assert_eq!(
            app.terminal_lines(terminal),
            vec!["main".to_string(), "keep".to_string()]
        );
    }

    #[test]
    fn terminal_render_lines_preserve_styled_trailing_spaces() {
        let mut app = App::default();
        let terminal = TerminalId(102);
        app.resize_terminal_buffer(terminal, 1, 8);

        app.append_terminal_output(terminal, "\x1b[44m    \x1b[0m");

        let lines = app.terminal_render_lines(terminal);
        assert_eq!(lines[0].spans[0].text, "    ");
        assert_eq!(lines[0].spans[0].style.bg, Some(TerminalColor::Blue));
    }

    #[test]
    fn terminal_render_lines_show_cursor() {
        let mut app = App::default();
        let terminal = TerminalId(103);
        app.resize_terminal_buffer(terminal, 1, 4);

        app.append_terminal_output(terminal, "ok");

        let lines = app.terminal_render_lines(terminal);
        assert_eq!(lines[0].spans[0].text, "ok");
        assert_eq!(lines[0].spans[1].text, "▌");
        assert_eq!(lines[0].spans[1].style.fg, Some(TerminalColor::Black));
        assert_eq!(lines[0].spans[1].style.bg, Some(TerminalColor::BrightWhite));
    }

    #[test]
    fn terminal_buffer_tracks_cursor_visibility_mode() {
        let mut buffer = TerminalBuffer::default();
        buffer.resize(1, 4);
        buffer.append("ok\x1b[?25l");

        assert_eq!(buffer.render_lines()[0].spans[0].text, "ok");
        assert!(!buffer.snapshot().cursor.unwrap().visible);

        buffer.append("\x1b[?25h");

        assert!(buffer.snapshot().cursor.unwrap().visible);
        assert_eq!(buffer.render_lines()[0].spans[1].text, "▌");
    }

    #[test]
    fn selected_input_terminal_forces_cursor_visible() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        app.resize_terminal_buffer(terminal, 1, 4);
        app.append_terminal_output(terminal, "ok\x1b[?25l");

        assert_eq!(rendered_text(app.terminal_render_lines(terminal)), "ok");

        assert!(app.begin_terminal_input());

        assert_eq!(rendered_text(app.terminal_render_lines(terminal)), "ok▌");
    }

    #[test]
    fn terminal_buffer_preserves_reverse_video_style() {
        let mut app = App::default();
        let terminal = TerminalId(119);
        app.resize_terminal_buffer(terminal, 1, 8);

        app.append_terminal_output(terminal, "a\x1b[7mb\x1b[27mc");

        let lines = app.terminal_render_lines(terminal);
        assert_eq!(lines[0].spans[0].text, "a");
        assert!(!lines[0].spans[0].style.reversed);
        assert_eq!(lines[0].spans[1].text, "b");
        assert!(lines[0].spans[1].style.reversed);
        assert_eq!(lines[0].spans[2].text, "c");
        assert!(!lines[0].spans[2].style.reversed);
    }

    #[test]
    fn terminal_buffer_reports_device_status_and_cursor_position() {
        let mut buffer = TerminalBuffer::default();
        buffer.resize(3, 8);

        let responses = buffer.append_and_collect_responses("ab\x1b[2;4H\x1b[5n\x1b[6n\x1b[c\x1bZ");

        assert_eq!(
            responses,
            vec![
                b"\x1b[0n".to_vec(),
                b"\x1b[2;4R".to_vec(),
                b"\x1b[?1;2c".to_vec(),
                b"\x1b[?1;2c".to_vec(),
            ]
        );
    }

    #[test]
    fn terminal_buffer_wraps_paste_when_bracketed_paste_is_enabled() {
        let mut buffer = TerminalBuffer::default();

        assert_eq!(buffer.paste_bytes("one\ntwo"), b"one\ntwo".to_vec());

        buffer.append("\x1b[?2004h");
        assert_eq!(
            buffer.paste_bytes("one\ntwo"),
            b"\x1b[200~one\ntwo\x1b[201~".to_vec()
        );

        buffer.append("\x1b[?2004l");
        assert_eq!(buffer.paste_bytes("one\ntwo"), b"one\ntwo".to_vec());
    }

    #[test]
    fn terminal_buffer_tracks_application_cursor_key_mode() {
        let mut buffer = TerminalBuffer::default();

        assert!(!buffer.application_cursor_keys_enabled());
        buffer.append("\x1b[?1h");
        assert!(buffer.application_cursor_keys_enabled());
        buffer.append("\x1b[?1l");
        assert!(!buffer.application_cursor_keys_enabled());
    }

    #[test]
    fn new_chats_use_agent_title() {
        let mut app = App::default();
        let Some((workspace, chat)) = app.add_chat_to_selected_workspace_and_return() else {
            panic!("chat should be added");
        };

        assert_eq!(
            app.project.chat(workspace, chat).unwrap().name,
            DEFAULT_AGENT_CHAT_TITLE
        );
    }

    #[test]
    fn app_normalizes_chat_titles_to_agent_on_load() {
        let mut state = ProjectState::default();
        state.workspaces[0].chats[0].name = "pi: old topic title".to_string();
        let app = App::new(state);

        assert_eq!(
            app.project.workspaces[0].chats[0].name,
            DEFAULT_AGENT_CHAT_TITLE
        );
        assert!(app.is_dirty());
    }

    #[test]
    fn app_repairs_low_allocators_on_load() {
        let state = ProjectState {
            next_workspace_id: 1,
            next_chat_id: 1,
            next_terminal_id: 1,
            ..ProjectState::default()
        };

        let app = App::new(state);

        assert_eq!(app.project.next_workspace_id, 3);
        assert_eq!(app.project.next_chat_id, 4);
        assert_eq!(app.project.next_terminal_id, 3);
        assert!(app.is_dirty());
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
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.is_dirty());
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
        app.resize_terminal_buffer(terminal, 2, 12);
        app.append_terminal_output(terminal, "alpha\r\nbeta\r\ngamma");

        assert!(app.begin_search());
        for ch in "alp".chars() {
            app.push_prompt_char(ch);
        }
        app.submit_search();

        assert_eq!(
            app.filtered_terminal_lines(terminal),
            Some(vec!["alpha".to_string()])
        );
        assert!(app.search_status().unwrap().contains("1 match"));
    }

    #[test]
    fn chat_search_filters_persisted_transcript_lines() {
        let mut state = ProjectState::default();
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
    fn delete_selected_terminal_removes_it_immediately() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        app.resize_terminal_buffer(terminal, 1, 10);

        let runtime_terminals = app.delete_selected_immediately();

        assert_eq!(runtime_terminals, vec![terminal]);
        assert!(app.project.terminal(workspace, terminal).is_none());
        assert!(!app.terminal_buffers.contains_key(&terminal));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.is_dirty());
    }

    #[test]
    fn delete_selected_chat_removes_transcript_and_pi_runtime_id() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });
        app.chat_buffers.insert(chat, ChatBuffer::default());
        let pi_terminal = chat_agent_terminal_id(chat);
        app.resize_terminal_buffer(pi_terminal, 1, 10);

        let runtime_terminals = app.delete_selected_immediately();

        assert_eq!(runtime_terminals, vec![pi_terminal]);
        assert!(app.project.chat(workspace, chat).is_none());
        assert!(!app.chat_buffers.contains_key(&chat));
        assert!(!app.terminal_buffers.contains_key(&pi_terminal));
        assert!(app.is_dirty());
    }

    #[test]
    fn delete_selected_workspace_removes_nested_runtime_ids() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        let chats = app.project.workspaces[0]
            .chats
            .iter()
            .map(|chat| chat.id)
            .collect::<Vec<_>>();
        app.select_item(NavItem::Workspace(workspace));

        let runtime_terminals = app.delete_selected_immediately();

        assert!(runtime_terminals.contains(&terminal));
        for chat in chats {
            assert!(runtime_terminals.contains(&chat_agent_terminal_id(chat)));
        }
        assert!(app.project.workspace(workspace).is_none());
        assert!(app.is_dirty());
    }

    #[test]
    fn unchanged_status_updates_do_not_mark_dirty() {
        let mut app = App::default();
        let terminal = app.project.workspaces[0].terminals[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        let chat_status = app.project.workspaces[0].chats[0].status;
        app.mark_clean();

        app.mark_terminal_stopped(terminal);
        app.mark_chat_status_by_id(chat, chat_status);

        assert!(!app.is_dirty());
    }

    #[test]
    fn non_terminal_selection_does_not_enter_input_mode() {
        let mut app = App::default();

        assert!(!app.begin_terminal_input());
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn agent_message_event_appends_chat_transcript() {
        let mut app = App::default();
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
        let mut state = ProjectState::default();
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
        let mut app = App::default();
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
        app.begin_open_workspace();
        if let Some(Prompt::OpenWorkspace(prompt)) = &mut app.prompt {
            prompt.input.clear();
        }

        app.push_prompt_char('/');
        app.push_prompt_char('t');
        app.pop_prompt_char();

        assert_eq!(
            app.prompt,
            Some(Prompt::OpenWorkspace(OpenWorkspacePrompt {
                input: "/".to_string(),
                error: None,
            }))
        );
    }

    #[test]
    fn importing_workspace_adds_cwd_chat_and_terminal() {
        let path = unique_temp_dir();
        let mut app = App::default();
        app.begin_open_workspace();
        if let Some(Prompt::OpenWorkspace(prompt)) = &mut app.prompt {
            prompt.input = path.display().to_string();
        }

        app.submit_open_workspace();

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.cwd.as_deref(), Some(path.as_path()));
        assert_eq!(imported.chats.len(), 1);
        assert_eq!(imported.terminals.len(), 1);
        assert_eq!(app.selected_item(), Some(NavItem::Workspace(imported.id)));
        assert_eq!(app.prompt, None);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.is_dirty());
    }

    #[test]
    fn invalid_import_stays_in_prompt() {
        let mut app = App::default();
        app.begin_open_workspace();
        if let Some(Prompt::OpenWorkspace(prompt)) = &mut app.prompt {
            prompt.input = "/this/path/should/not/exist".to_string();
        }

        app.submit_open_workspace();

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
}
