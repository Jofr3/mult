use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::model::{ChatId, ChatStatus, ProjectState, TerminalId, TerminalStatus, WorkspaceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub project: ProjectState,
    pub selected: usize,
    pub mode: Mode,
    pub terminal_buffers: BTreeMap<TerminalId, TerminalBuffer>,
    pub should_quit: bool,
    dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    OpenWorkspace(OpenWorkspacePrompt),
    NewTerminalCommand(TerminalCommandPrompt),
    TerminalInput {
        workspace: WorkspaceId,
        terminal: TerminalId,
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalBuffer {
    lines: Vec<String>,
    partial: String,
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

impl App {
    pub fn new(mut project: ProjectState) -> Self {
        project.reset_terminal_statuses();
        let mut app = Self {
            project,
            selected: 0,
            mode: Mode::Normal,
            terminal_buffers: BTreeMap::new(),
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
            Mode::OpenWorkspace(_) | Mode::NewTerminalCommand(_)
        )
    }

    pub fn terminal_input_target(&self) -> Option<TerminalId> {
        match self.mode {
            Mode::TerminalInput { terminal, .. } => Some(terminal),
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

    pub fn end_terminal_input(&mut self) {
        self.mode = Mode::Normal;
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

    pub fn add_chat_to_selected_workspace(&mut self) {
        let Some(workspace) = self.selected_workspace_id() else {
            return;
        };
        let next = self
            .project
            .workspace(workspace)
            .map(|workspace| workspace.chats.len() + 1)
            .unwrap_or(1);

        if let Some(chat) =
            self.project
                .add_chat(workspace, format!("agent: chat-{next}"), ChatStatus::Idle)
        {
            self.select_item(NavItem::Chat { workspace, chat });
            self.dirty = true;
        }
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
    const MAX_LINES: usize = 500;

    fn append(&mut self, text: &str) {
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                if chars.next_if_eq(&'[').is_some() {
                    for code in chars.by_ref() {
                        if ('@'..='~').contains(&code) {
                            break;
                        }
                    }
                }
                continue;
            }

            match ch {
                '\n' => self.flush_partial(),
                '\r' => self.partial.clear(),
                '\t' => self.partial.push(' '),
                ch if ch.is_control() => {}
                ch => self.partial.push(ch),
            }
        }
    }

    fn visible_lines(&self) -> Vec<String> {
        let mut lines = self.lines.clone();
        if !self.partial.is_empty() {
            lines.push(self.partial.clone());
        }
        lines
    }

    fn flush_partial(&mut self) {
        let line = std::mem::take(&mut self.partial);
        self.push_line(line);
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
    fn adding_chat_selects_the_new_chat() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;

        app.add_chat_to_selected_workspace();

        let chat = app.project.workspaces[0].chats.last().unwrap().id;
        assert_eq!(app.selected_item(), Some(NavItem::Chat { workspace, chat }));
        assert!(app.is_dirty());
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
        assert_eq!(
            app.mode,
            Mode::TerminalInput {
                workspace,
                terminal,
            }
        );
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
    fn non_terminal_selection_does_not_enter_input_mode() {
        let mut app = App::default();

        assert!(!app.begin_terminal_input());
        assert_eq!(app.mode, Mode::Normal);
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
