use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    agent::{AgentEvent, AgentMessageRole, AgentTarget},
    model::{
        ChatId, ChatMessage, ChatMessageRole, ChatStatus, ProjectState, TerminalId, TerminalStatus,
        WorkspaceId,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub project: ProjectState,
    pub selected: usize,
    pub mode: Mode,
    pub terminal_buffers: BTreeMap<TerminalId, TerminalBuffer>,
    pub chat_buffers: BTreeMap<ChatId, ChatBuffer>,
    pub should_quit: bool,
    dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    OpenWorkspace(OpenWorkspacePrompt),
    NewTerminalCommand(TerminalCommandPrompt),
    ConfirmDelete(DeleteConfirmation),
    TerminalInput {
        workspace: WorkspaceId,
        terminal: TerminalId,
    },
    ChatAgentInput {
        workspace: WorkspaceId,
        chat: ChatId,
    },
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
pub struct DeleteConfirmation {
    pub target: DeleteTarget,
    pub label: String,
    pub detail: String,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalScreen {
    rows: u16,
    cols: u16,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    current_style: TerminalCellStyle,
    cells: Vec<Vec<TerminalCell>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum TerminalParser {
    #[default]
    Ground,
    Escape,
    Csi(String),
    Osc {
        esc_seen: bool,
    },
    IgnoreOne,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalCellStyle {
    pub fg: Option<TerminalColor>,
    pub bg: Option<TerminalColor>,
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TerminalCell {
    ch: char,
    style: TerminalCellStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRenderLine {
    pub spans: Vec<TerminalRenderSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRenderSpan {
    pub text: String,
    pub style: TerminalCellStyle,
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
    TerminalId(chat.0 | (1 << 63))
}

pub fn chat_id_from_agent_terminal_id(terminal: TerminalId) -> Option<ChatId> {
    ((terminal.0 & (1 << 63)) != 0).then_some(ChatId(terminal.0 & !(1 << 63)))
}

impl App {
    pub fn new(mut project: ProjectState) -> Self {
        project.reset_terminal_statuses();
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
            terminal_buffers: BTreeMap::new(),
            chat_buffers,
            should_quit: false,
            dirty: false,
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
        matches!(
            self.mode,
            Mode::OpenWorkspace(_) | Mode::NewTerminalCommand(_) | Mode::ConfirmDelete(_)
        )
    }

    pub fn terminal_input_target(&self) -> Option<TerminalId> {
        match self.mode {
            Mode::TerminalInput { terminal, .. } => Some(terminal),
            _ => None,
        }
    }

    pub fn pty_input_target(&self) -> Option<TerminalId> {
        match self.mode {
            Mode::TerminalInput { terminal, .. } => Some(terminal),
            Mode::ChatAgentInput { chat, .. } => Some(chat_agent_terminal_id(chat)),
            _ => None,
        }
    }

    pub fn nav_items(&self) -> Vec<NavItem> {
        let mut items = Vec::new();

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

    pub fn selected_item(&self) -> Option<NavItem> {
        self.nav_items().get(self.selected).copied()
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

    pub fn begin_delete_selected(&mut self) -> bool {
        let Some(target) = self.selected_delete_target() else {
            return false;
        };
        let Some((label, detail)) = self.delete_target_description(target) else {
            return false;
        };

        self.mode = Mode::ConfirmDelete(DeleteConfirmation {
            target,
            label,
            detail,
        });
        true
    }

    pub fn confirm_delete_selected(&mut self) -> Vec<TerminalId> {
        let Mode::ConfirmDelete(confirmation) = &self.mode else {
            return Vec::new();
        };
        let target = confirmation.target;
        self.mode = Mode::Normal;

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
                    self.dirty = true;
                }
            }
        }

        self.clamp_selection();
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

    fn delete_target_description(&self, target: DeleteTarget) -> Option<(String, String)> {
        match target {
            DeleteTarget::Workspace(workspace_id) => {
                let workspace = self.project.workspace(workspace_id)?;
                Some((
                    format!("workspace `{}`", workspace.name),
                    format!(
                        "Deletes {} agent chat(s) and {} terminal(s).",
                        workspace.chats.len(),
                        workspace.terminals.len()
                    ),
                ))
            }
            DeleteTarget::Chat { workspace, chat } => {
                let chat = self.project.chat(workspace, chat)?;
                Some((
                    format!("agent `{}`", chat.name),
                    "Deletes saved transcript and stops its pi agent if running.".to_string(),
                ))
            }
            DeleteTarget::Terminal {
                workspace,
                terminal,
            } => {
                let terminal = self.project.terminal(workspace, terminal)?;
                Some((
                    format!("terminal `{}`", terminal.name),
                    "Deletes terminal config and stops its PTY if running.".to_string(),
                ))
            }
        }
    }

    pub fn mark_terminal_running(&mut self, terminal: TerminalId) {
        if let Some(terminal) = self.project.terminal_mut_by_id(terminal) {
            terminal.status = TerminalStatus::Running;
        }
    }

    pub fn mark_terminal_stopped(&mut self, terminal: TerminalId) {
        if let Some(terminal) = self.project.terminal_mut_by_id(terminal) {
            terminal.status = TerminalStatus::Stopped;
        }
    }

    pub fn append_terminal_output(&mut self, terminal: TerminalId, text: &str) {
        self.terminal_buffers
            .entry(terminal)
            .or_default()
            .append(text);
    }

    pub fn append_terminal_system_line(
        &mut self,
        terminal: TerminalId,
        message: impl Into<String>,
    ) {
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

    pub fn terminal_render_lines(&self, terminal: TerminalId) -> Vec<TerminalRenderLine> {
        self.terminal_buffers
            .get(&terminal)
            .map(TerminalBuffer::render_lines)
            .unwrap_or_default()
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
                    chat.status = status;
                    self.dirty = true;
                }
            }
            AgentEvent::Error { target, message } => {
                if let Some(chat) = self.project.chat_mut(target.workspace, target.chat) {
                    chat.status = ChatStatus::Failed;
                    self.dirty = true;
                }
                self.append_chat_message(target, ChatMessageRole::Error, message);
            }
        }
    }

    pub fn select_next(&mut self) {
        let len = self.nav_items().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    pub fn select_previous(&mut self) {
        let len = self.nav_items().len();
        if len > 0 {
            self.selected = self.selected.checked_sub(1).unwrap_or(len - 1);
        }
    }

    pub fn add_workspace(&mut self) {
        let next = self.project.workspaces.len() + 1;
        let workspace = self
            .project
            .add_workspace(format!("workspace-{next}"), std::env::current_dir().ok());
        let chat =
            self.project
                .add_chat(workspace, "agent: new chat".to_string(), ChatStatus::Idle);

        if let Some(chat) = chat {
            self.select_item(NavItem::Chat { workspace, chat });
        } else {
            self.select_item(NavItem::Workspace(workspace));
        }
        self.dirty = true;
    }

    pub fn begin_open_workspace(&mut self) {
        let input = std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        self.mode = Mode::OpenWorkspace(OpenWorkspacePrompt { input, error: None });
    }

    pub fn begin_new_terminal_command(&mut self) -> bool {
        if self.selected_workspace_id().is_none() {
            return false;
        }

        self.mode = Mode::NewTerminalCommand(TerminalCommandPrompt {
            input: String::new(),
            error: None,
        });
        true
    }

    pub fn cancel_prompt(&mut self) {
        self.mode = Mode::Normal;
    }

    pub fn begin_terminal_input(&mut self) -> bool {
        let Some((workspace, terminal)) = self.selected_terminal_id() else {
            return false;
        };

        self.mode = Mode::TerminalInput {
            workspace,
            terminal,
        };
        true
    }

    pub fn begin_chat_agent_input(&mut self) -> bool {
        let Some((workspace, chat)) = self.selected_chat_id() else {
            return false;
        };

        self.mode = Mode::ChatAgentInput { workspace, chat };
        true
    }

    pub fn end_terminal_input(&mut self) {
        self.mode = Mode::Normal;
    }

    pub fn end_pty_input(&mut self) {
        self.mode = Mode::Normal;
    }

    pub fn mark_chat_status_by_id(&mut self, chat: ChatId, status: ChatStatus) {
        for chat_session in self
            .project
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.chats.iter_mut())
        {
            if chat_session.id == chat {
                chat_session.status = status;
                self.dirty = true;
                return;
            }
        }
    }

    pub fn push_prompt_char(&mut self, c: char) {
        match &mut self.mode {
            Mode::OpenWorkspace(prompt) => {
                prompt.input.push(c);
                prompt.error = None;
            }
            Mode::NewTerminalCommand(prompt) => {
                prompt.input.push(c);
                prompt.error = None;
            }
            _ => {}
        }
    }

    pub fn pop_prompt_char(&mut self) {
        match &mut self.mode {
            Mode::OpenWorkspace(prompt) => {
                prompt.input.pop();
                prompt.error = None;
            }
            Mode::NewTerminalCommand(prompt) => {
                prompt.input.pop();
                prompt.error = None;
            }
            _ => {}
        }
    }

    pub fn submit_open_workspace(&mut self) {
        let Mode::OpenWorkspace(prompt) = &self.mode else {
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
            self.mode = Mode::Normal;
            self.select_item(NavItem::Workspace(existing_workspace.id));
            return;
        }

        let name = workspace_name(&cwd);
        let workspace = self.project.add_workspace(name, Some(cwd));
        self.project
            .add_chat(workspace, "agent: new chat".to_string(), ChatStatus::Idle);
        self.project
            .add_terminal(workspace, "shell".to_string(), TerminalStatus::Stopped);

        self.mode = Mode::Normal;
        self.select_item(NavItem::Workspace(workspace));
        self.dirty = true;
    }

    pub fn submit_new_terminal_command(&mut self) {
        let Mode::NewTerminalCommand(prompt) = &self.mode else {
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
                .add_command_terminal(workspace, name, TerminalStatus::Stopped, command)
        {
            self.mode = Mode::Normal;
            self.select_item(NavItem::Terminal {
                workspace,
                terminal,
            });
            self.dirty = true;
        }
    }

    pub fn add_chat_to_selected_workspace_and_return(&mut self) -> Option<(WorkspaceId, ChatId)> {
        let workspace = self.selected_workspace_id()?;
        let next = self
            .project
            .workspace(workspace)
            .map(|workspace| workspace.chats.len() + 1)
            .unwrap_or(1);

        let chat =
            self.project
                .add_chat(workspace, format!("agent: chat-{next}"), ChatStatus::Idle)?;
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

        if let Some(terminal) = self.project.add_terminal(
            workspace,
            format!("terminal-{next}"),
            TerminalStatus::Stopped,
        ) {
            self.select_item(NavItem::Terminal {
                workspace,
                terminal,
            });
            self.dirty = true;
        }
    }

    pub fn rotate_selected_status(&mut self) {
        match self.selected_item() {
            Some(NavItem::Chat { workspace, chat }) => {
                if let Some(chat) = self.project.chat_mut(workspace, chat) {
                    chat.status = chat.status.next();
                    self.dirty = true;
                }
            }
            Some(NavItem::Terminal {
                workspace,
                terminal,
            }) => {
                if let Some(terminal) = self.project.terminal_mut(workspace, terminal) {
                    terminal.status = terminal.status.next();
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    fn set_open_workspace_error(&mut self, message: impl Into<String>) {
        if let Mode::OpenWorkspace(prompt) = &mut self.mode {
            prompt.error = Some(message.into());
        }
    }

    fn set_terminal_command_error(&mut self, message: impl Into<String>) {
        if let Mode::NewTerminalCommand(prompt) = &mut self.mode {
            prompt.error = Some(message.into());
        }
    }

    fn select_item(&mut self, target: NavItem) {
        if let Some(index) = self.nav_items().iter().position(|item| *item == target) {
            self.selected = index;
        } else {
            self.clamp_selection();
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.nav_items().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

impl TerminalBuffer {
    fn append(&mut self, text: &str) {
        for ch in text.chars() {
            self.process_char(ch);
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.screen.resize(rows.max(1), cols.max(1));
    }

    fn visible_lines(&self) -> Vec<String> {
        self.screen.visible_lines()
    }

    fn render_lines(&self) -> Vec<TerminalRenderLine> {
        self.screen.render_lines()
    }

    fn push_line(&mut self, line: String) {
        self.append(&line);
        self.screen.carriage_return();
        self.screen.line_feed();
    }

    fn process_char(&mut self, ch: char) {
        let state = std::mem::take(&mut self.parser);
        self.parser = match state {
            TerminalParser::Ground => self.process_ground_char(ch),
            TerminalParser::Escape => self.process_escape_char(ch),
            TerminalParser::Csi(mut sequence) => {
                if ('@'..='~').contains(&ch) {
                    self.apply_csi(&sequence, ch);
                    TerminalParser::Ground
                } else {
                    sequence.push(ch);
                    TerminalParser::Csi(sequence)
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

    fn process_escape_char(&mut self, ch: char) -> TerminalParser {
        match ch {
            '[' => TerminalParser::Csi(String::new()),
            ']' => TerminalParser::Osc { esc_seen: false },
            '(' | ')' | '*' | '+' => TerminalParser::IgnoreOne,
            'c' => {
                self.screen.clear();
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

    fn apply_csi(&mut self, sequence: &str, final_char: char) {
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
            'm' => self.screen.apply_sgr(&params),
            'S' => self.screen.scroll_up(param_or_default(&params, 0, 1)),
            'T' => self.screen.scroll_down(param_or_default(&params, 0, 1)),
            'd' => self.screen.set_cursor_row(param_or_default(&params, 0, 1)),
            's' => self.screen.save_cursor(),
            'u' => self.screen.restore_cursor(),
            'h' if private
                && params
                    .iter()
                    .any(|param| matches!(*param, 47 | 1047 | 1049)) =>
            {
                self.screen.clear();
            }
            'l' if private
                && params
                    .iter()
                    .any(|param| matches!(*param, 47 | 1047 | 1049)) =>
            {
                self.screen.clear();
            }
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
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            current_style: TerminalCellStyle::default(),
            cells: vec![vec![TerminalCell::blank(); usize::from(cols)]; usize::from(rows)],
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        self.rows = rows;
        self.cols = cols;
        let row_len = usize::from(cols);
        let row_count = usize::from(rows);
        self.cells.resize(row_count, Vec::new());
        for row in &mut self.cells {
            row.resize(row_len, TerminalCell::blank());
        }
        self.clamp_cursor();
    }

    fn visible_lines(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn render_lines(&self) -> Vec<TerminalRenderLine> {
        self.cells
            .iter()
            .enumerate()
            .map(|(row_index, row)| {
                render_terminal_row(
                    row,
                    (row_index == self.cursor_row).then_some(self.cursor_col),
                )
            })
            .collect()
    }

    fn put_char(&mut self, ch: char) {
        if self.cursor_col >= usize::from(self.cols) {
            self.carriage_return();
            self.line_feed();
        }
        self.cells[self.cursor_row][self.cursor_col] = TerminalCell {
            ch,
            style: self.current_style,
        };
        self.cursor_col += 1;
        if self.cursor_col >= usize::from(self.cols) {
            self.cursor_col = usize::from(self.cols).saturating_sub(1);
        }
    }

    fn line_feed(&mut self) {
        if self.cursor_row + 1 >= usize::from(self.rows) {
            self.scroll_up(1);
        } else {
            self.cursor_row += 1;
        }
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    fn tab(&mut self) {
        let next_tab = ((self.cursor_col / 8) + 1) * 8;
        self.cursor_col = next_tab.min(usize::from(self.cols).saturating_sub(1));
    }

    fn backspace(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    fn reverse_index(&mut self) {
        self.cursor_row = self.cursor_row.saturating_sub(1);
    }

    fn move_cursor_up(&mut self, count: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(count);
    }

    fn move_cursor_down(&mut self, count: usize) {
        self.cursor_row = (self.cursor_row + count).min(usize::from(self.rows).saturating_sub(1));
    }

    fn move_cursor_right(&mut self, count: usize) {
        self.cursor_col = (self.cursor_col + count).min(usize::from(self.cols).saturating_sub(1));
    }

    fn move_cursor_left(&mut self, count: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(count);
    }

    fn set_cursor_position(&mut self, row: usize, col: usize) {
        self.cursor_row = row
            .saturating_sub(1)
            .min(usize::from(self.rows).saturating_sub(1));
        self.cursor_col = col
            .saturating_sub(1)
            .min(usize::from(self.cols).saturating_sub(1));
    }

    fn set_cursor_row(&mut self, row: usize) {
        self.cursor_row = row
            .saturating_sub(1)
            .min(usize::from(self.rows).saturating_sub(1));
    }

    fn set_cursor_col(&mut self, col: usize) {
        self.cursor_col = col
            .saturating_sub(1)
            .min(usize::from(self.cols).saturating_sub(1));
    }

    fn erase_display(&mut self, mode: usize) {
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
            2 | 3 => self.clear(),
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        match mode {
            0 => self.erase_line_from_cursor(),
            1 => self.erase_line_to_cursor(),
            2 => self.clear_row(self.cursor_row),
            _ => {}
        }
    }

    fn scroll_up(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            if !self.cells.is_empty() {
                self.cells.remove(0);
                self.cells
                    .push(vec![TerminalCell::blank(); usize::from(self.cols)]);
            }
        }
    }

    fn scroll_down(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            if !self.cells.is_empty() {
                self.cells.pop();
                self.cells
                    .insert(0, vec![TerminalCell::blank(); usize::from(self.cols)]);
            }
        }
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_row, self.cursor_col));
    }

    fn restore_cursor(&mut self) {
        if let Some((row, col)) = self.saved_cursor {
            self.cursor_row = row;
            self.cursor_col = col;
            self.clamp_cursor();
        }
    }

    fn clear(&mut self) {
        for row in 0..usize::from(self.rows) {
            self.clear_row(row);
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    fn erase_line_from_cursor(&mut self) {
        let cols = usize::from(self.cols);
        for col in self.cursor_col..cols {
            self.cells[self.cursor_row][col] = TerminalCell::blank();
        }
    }

    fn erase_line_to_cursor(&mut self) {
        for col in 0..=self.cursor_col {
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
                22 => self.current_style.bold = false,
                23 => self.current_style.italic = false,
                24 => self.current_style.underlined = false,
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
    }
}

impl TerminalCell {
    fn blank() -> Self {
        Self {
            ch: ' ',
            style: TerminalCellStyle::default(),
        }
    }
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
    style.fg = Some(TerminalColor::BrightWhite);
    style.bg = None;
    style.underlined = false;
    style
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
        [2, red, green, blue, ..] => {
            Some((TerminalColor::Rgb(*red as u8, *green as u8, *blue as u8), 4))
        }
        [5, index, ..] => Some((xterm_256_color(*index), 2)),
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

fn command_terminal_name(command: &str, next: usize) -> String {
    command
        .split_whitespace()
        .next()
        .filter(|name| !name.is_empty())
        .map(|name| format!("cmd: {name}"))
        .unwrap_or_else(|| format!("command-{next}"))
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
    fn navigation_contains_nested_workspace_items_with_stable_ids() {
        let app = App::default();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;

        assert_eq!(app.nav_items()[0], NavItem::Workspace(workspace));
        assert_eq!(app.nav_items()[1], NavItem::Chat { workspace, chat });
        assert!(app
            .nav_items()
            .iter()
            .any(|item| matches!(item, NavItem::Terminal { .. })));
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
            Mode::TerminalInput {
                workspace,
                terminal,
            }
        );
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
        assert_eq!(app.mode, Mode::ChatAgentInput { workspace, chat });
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
        assert_eq!(lines[0].spans[1].style.fg, Some(TerminalColor::BrightWhite));
        assert_eq!(lines[0].spans[1].style.bg, None);
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
        assert_eq!(terminal.name, "cmd: cargo");
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
    fn delete_selected_terminal_requires_confirmation_and_removes_it() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        app.resize_terminal_buffer(terminal, 1, 10);

        assert!(app.begin_delete_selected());
        assert!(matches!(
            app.mode,
            Mode::ConfirmDelete(DeleteConfirmation {
                target: DeleteTarget::Terminal { .. },
                ..
            })
        ));

        let runtime_terminals = app.confirm_delete_selected();

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

        assert!(app.begin_delete_selected());
        let runtime_terminals = app.confirm_delete_selected();

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

        assert!(app.begin_delete_selected());
        let runtime_terminals = app.confirm_delete_selected();

        assert!(runtime_terminals.contains(&terminal));
        for chat in chats {
            assert!(runtime_terminals.contains(&chat_agent_terminal_id(chat)));
        }
        assert!(app.project.workspace(workspace).is_none());
        assert!(app.is_dirty());
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
    fn rotating_selected_chat_status_marks_app_dirty() {
        let mut app = App {
            selected: 1,
            ..App::default()
        };
        let Some(NavItem::Chat { workspace, chat }) = app.selected_item() else {
            panic!("expected a chat selection");
        };

        app.rotate_selected_status();

        assert_eq!(
            app.project.chat(workspace, chat).unwrap().status,
            ChatStatus::Waiting
        );
        assert!(app.is_dirty());
    }

    #[test]
    fn prompt_input_can_be_edited() {
        let mut app = App::default();
        app.begin_open_workspace();
        if let Mode::OpenWorkspace(prompt) = &mut app.mode {
            prompt.input.clear();
        }

        app.push_prompt_char('/');
        app.push_prompt_char('t');
        app.pop_prompt_char();

        assert_eq!(
            app.mode,
            Mode::OpenWorkspace(OpenWorkspacePrompt {
                input: "/".to_string(),
                error: None,
            })
        );
    }

    #[test]
    fn importing_workspace_adds_cwd_chat_and_terminal() {
        let path = unique_temp_dir();
        let mut app = App::default();
        app.begin_open_workspace();
        if let Mode::OpenWorkspace(prompt) = &mut app.mode {
            prompt.input = path.display().to_string();
        }

        app.submit_open_workspace();

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.cwd.as_deref(), Some(path.as_path()));
        assert_eq!(imported.chats.len(), 1);
        assert_eq!(imported.terminals.len(), 1);
        assert_eq!(app.selected_item(), Some(NavItem::Workspace(imported.id)));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.is_dirty());
    }

    #[test]
    fn invalid_import_stays_in_prompt() {
        let mut app = App::default();
        app.begin_open_workspace();
        if let Mode::OpenWorkspace(prompt) = &mut app.mode {
            prompt.input = "/this/path/should/not/exist".to_string();
        }

        app.submit_open_workspace();

        let Mode::OpenWorkspace(prompt) = &app.mode else {
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
