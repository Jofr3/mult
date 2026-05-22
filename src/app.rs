use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    agent::{AgentEvent, AgentMessageRole, AgentTarget},
    config::ConfiguredProject,
    model::{
        ChatId, ChatMessage, ChatMessageRole, ChatStatus, ProjectState, TerminalId, TerminalStatus,
        WorkspaceId, DEFAULT_AGENT_CHAT_TITLE, RUNTIME_TERMINAL_ID_FLAG,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub project: ProjectState,
    pub selected: usize,
    pub prompt: Option<Prompt>,
    pub focus: FocusMode,
    pub chat_buffers: BTreeMap<ChatId, ChatBuffer>,
    pub active_search: Option<SearchState>,
    pub text_selection: Option<TextSelection>,
    pub should_quit: bool,
    dirty: bool,
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
    pub selected: usize,
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
pub struct SelectionCell {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub terminal: TerminalId,
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
            prompt: None,
            focus: FocusMode::Sidebar,
            chat_buffers,
            active_search: None,
            text_selection: None,
            should_quit: false,
            dirty: ids_normalized || titles_normalized,
        };
        app.clamp_selection();
        app.sync_focus_to_selection();
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

    pub fn open_workspace_matches(
        &self,
        projects: &[ConfiguredProject],
    ) -> Vec<OpenWorkspaceMatch> {
        if let Some(Prompt::OpenWorkspace(prompt)) = &self.prompt {
            if prompt.mode == OpenWorkspaceMode::ConfiguredProjects {
                return open_workspace_matches_for(&prompt.input, projects);
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

    pub fn begin_text_selection(&mut self, terminal: TerminalId, cell: SelectionCell) {
        self.text_selection = Some(TextSelection {
            terminal,
            anchor: cell,
            focus: cell,
            dragging: true,
        });
    }

    pub fn update_text_selection(&mut self, terminal: TerminalId, cell: SelectionCell) -> bool {
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
        terminal: TerminalId,
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

    pub fn text_selection_for(&self, terminal: TerminalId) -> Option<&TextSelection> {
        self.text_selection
            .as_ref()
            .filter(|selection| selection.terminal == terminal)
    }

    pub fn search_status(&self) -> Option<String> {
        let search = self.active_search.as_ref()?;
        match search.scope {
            SearchScope::Chat(_) => self.chat_search_status(),
            SearchScope::Terminal(_) => {
                Some(format!("search terminal: active for `{}`", search.query))
            }
        }
    }

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

    #[cfg(test)]
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
        self.sync_focus_to_selection();
    }

    fn sync_focus_to_selection(&mut self) {
        self.focus = self.selected_main_focus().unwrap_or(FocusMode::Sidebar);
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
                label: "Start selected PTY",
                help: "start the selected chat/terminal PTY for immediate input",
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

    fn move_open_workspace_selection(&mut self, delta: isize, projects: &[ConfiguredProject]) {
        let Some(Prompt::OpenWorkspace(prompt)) = &self.prompt else {
            return;
        };
        if prompt.mode != OpenWorkspaceMode::ConfiguredProjects {
            return;
        }
        let len = open_workspace_matches_for(&prompt.input, projects).len();
        if len == 0 {
            if let Some(Prompt::OpenWorkspace(prompt)) = &mut self.prompt {
                prompt.selected = 0;
            }
            return;
        }

        if let Some(Prompt::OpenWorkspace(prompt)) = &mut self.prompt {
            if delta.is_negative() {
                let delta = delta.unsigned_abs() % len;
                prompt.selected = prompt.selected.checked_sub(delta).unwrap_or(len - delta);
            } else {
                prompt.selected = (prompt.selected + delta as usize) % len;
            }
        }
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

    pub fn terminal_input_target(&self) -> Option<TerminalId> {
        self.selected_terminal_id().map(|(_, terminal)| terminal)
    }

    pub fn pty_input_target(&self) -> Option<TerminalId> {
        self.selected_output_terminal_id()
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

    pub fn selected_output_terminal_id(&self) -> Option<TerminalId> {
        match self.selected_item()? {
            NavItem::Chat { chat, .. } => Some(chat_agent_terminal_id(chat)),
            NavItem::Terminal { terminal, .. } => Some(terminal),
            NavItem::Workspace(_) => None,
        }
    }

    pub fn terminal_search_matches(
        &self,
        terminal: TerminalId,
        lines: Vec<String>,
    ) -> Option<Vec<String>> {
        let search = self.active_search.as_ref()?;
        if search.scope != SearchScope::Terminal(terminal) {
            return None;
        }
        Some(filter_lines(lines, &search.query))
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
            input,
            error: None,
            selected: 0,
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
            input: String::new(),
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

    pub fn end_terminal_input(&mut self) {
        self.sync_focus_to_selection();
    }

    pub fn end_pty_input(&mut self) {
        self.sync_focus_to_selection();
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
                prompt.selected = 0;
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
                prompt.selected = 0;
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

    pub fn submit_open_workspace(&mut self, projects: &[ConfiguredProject]) {
        let Some(Prompt::OpenWorkspace(prompt)) = &self.prompt else {
            return;
        };
        let raw_input = prompt.input.trim().to_string();
        let selected = prompt.selected;
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
            self.prompt = None;
            self.select_item(NavItem::Workspace(existing_workspace.id));
            return;
        }

        let name = configured_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| workspace_name(&cwd));
        let workspace = self.project.add_workspace(name, Some(cwd));
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
        assert_eq!(app.focus, FocusMode::Terminal);
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
        assert!(app.begin_search());
        for ch in "alp".chars() {
            app.push_prompt_char(ch);
        }
        app.submit_search();

        let lines = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        assert_eq!(
            app.terminal_search_matches(terminal, lines.clone()),
            Some(vec!["alpha".to_string()])
        );
        assert!(app
            .terminal_search_status(terminal, lines)
            .unwrap()
            .contains("1 match"));
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
        let runtime_terminals = app.delete_selected_immediately();

        assert_eq!(runtime_terminals, vec![terminal]);
        assert!(app.project.terminal(workspace, terminal).is_none());
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
        let runtime_terminals = app.delete_selected_immediately();

        assert_eq!(runtime_terminals, vec![pi_terminal]);
        assert!(app.project.chat(workspace, chat).is_none());
        assert!(!app.chat_buffers.contains_key(&chat));
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
    fn non_terminal_selection_is_not_a_terminal_input_target() {
        let mut app = App::default();

        assert!(!app.begin_terminal_input());
        assert_eq!(app.pty_input_target(), None);
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
        app.begin_open_workspace(&[]);
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
                selected: 0,
                mode: OpenWorkspaceMode::Path,
            }))
        );
    }

    #[test]
    fn importing_workspace_adds_cwd_chat_and_terminal() {
        let path = unique_temp_dir();
        let mut app = App::default();
        app.begin_open_workspace(&[]);
        if let Some(Prompt::OpenWorkspace(prompt)) = &mut app.prompt {
            prompt.input = path.display().to_string();
        }

        app.submit_open_workspace(&[]);

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.cwd.as_deref(), Some(path.as_path()));
        assert_eq!(imported.chats.len(), 1);
        assert_eq!(imported.terminals.len(), 1);
        assert_eq!(app.selected_item(), Some(NavItem::Workspace(imported.id)));
        assert_eq!(app.prompt, None);
        assert_eq!(app.focus, FocusMode::Sidebar);
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
        assert_eq!(app.selected_item(), Some(NavItem::Workspace(imported.id)));
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
            prompt.input = "/this/path/should/not/exist".to_string();
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
}
