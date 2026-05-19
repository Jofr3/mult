use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectState {
    pub version: u32,
    pub next_workspace_id: u64,
    pub next_chat_id: u64,
    pub next_terminal_id: u64,
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub cwd: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub chats: Vec<ChatSession>,
    pub terminals: Vec<TerminalSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: ChatId,
    pub name: String,
    pub status: ChatStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSession {
    pub id: TerminalId,
    pub name: String,
    pub status: TerminalStatus,
    #[serde(default)]
    pub launch: TerminalLaunch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChatId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TerminalId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatStatus {
    Idle,
    Thinking,
    Waiting,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalStatus {
    Stopped,
    Running,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "command", rename_all = "snake_case")]
pub enum TerminalLaunch {
    #[default]
    Shell,
    Command(String),
}

impl Default for ProjectState {
    fn default() -> Self {
        let mut state = Self {
            version: STATE_VERSION,
            next_workspace_id: 1,
            next_chat_id: 1,
            next_terminal_id: 1,
            workspaces: Vec::new(),
        };

        let mult = state.add_workspace("mult".to_string(), std::env::current_dir().ok());
        state.add_chat(mult, "agent: planner".to_string(), ChatStatus::Thinking);
        state.add_chat(mult, "agent: coder".to_string(), ChatStatus::Idle);
        state.add_terminal(mult, "dev server".to_string(), TerminalStatus::Running);

        let website = state.add_workspace("website".to_string(), None);
        state.add_chat(website, "agent: reviewer".to_string(), ChatStatus::Waiting);
        state.add_terminal(website, "shell".to_string(), TerminalStatus::Stopped);

        state
    }
}

impl ProjectState {
    pub fn add_workspace(&mut self, name: String, cwd: Option<PathBuf>) -> WorkspaceId {
        let id = self.allocate_workspace_id();
        self.workspaces.push(Workspace {
            id,
            name,
            cwd,
            environment: BTreeMap::new(),
            chats: Vec::new(),
            terminals: Vec::new(),
        });
        id
    }

    pub fn add_chat(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        status: ChatStatus,
    ) -> Option<ChatId> {
        let workspace_index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)?;
        let id = self.allocate_chat_id();
        self.workspaces[workspace_index]
            .chats
            .push(ChatSession { id, name, status });
        Some(id)
    }

    pub fn add_terminal(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        status: TerminalStatus,
    ) -> Option<TerminalId> {
        self.add_terminal_with_launch(workspace_id, name, status, TerminalLaunch::Shell)
    }

    pub fn add_command_terminal(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        status: TerminalStatus,
        command: String,
    ) -> Option<TerminalId> {
        self.add_terminal_with_launch(workspace_id, name, status, TerminalLaunch::Command(command))
    }

    fn add_terminal_with_launch(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        status: TerminalStatus,
        launch: TerminalLaunch,
    ) -> Option<TerminalId> {
        let workspace_index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)?;
        let id = self.allocate_terminal_id();
        self.workspaces[workspace_index]
            .terminals
            .push(TerminalSession {
                id,
                name,
                status,
                launch,
            });
        Some(id)
    }

    pub fn workspace(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|workspace| workspace.id == id)
    }

    pub fn workspace_mut(&mut self, id: WorkspaceId) -> Option<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .find(|workspace| workspace.id == id)
    }

    pub fn chat(&self, workspace_id: WorkspaceId, chat_id: ChatId) -> Option<&ChatSession> {
        self.workspace(workspace_id)?
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
    }

    pub fn chat_mut(
        &mut self,
        workspace_id: WorkspaceId,
        chat_id: ChatId,
    ) -> Option<&mut ChatSession> {
        self.workspace_mut(workspace_id)?
            .chats
            .iter_mut()
            .find(|chat| chat.id == chat_id)
    }

    pub fn terminal(
        &self,
        workspace_id: WorkspaceId,
        terminal_id: TerminalId,
    ) -> Option<&TerminalSession> {
        self.workspace(workspace_id)?
            .terminals
            .iter()
            .find(|terminal| terminal.id == terminal_id)
    }

    pub fn terminal_mut(
        &mut self,
        workspace_id: WorkspaceId,
        terminal_id: TerminalId,
    ) -> Option<&mut TerminalSession> {
        self.workspace_mut(workspace_id)?
            .terminals
            .iter_mut()
            .find(|terminal| terminal.id == terminal_id)
    }

    pub fn terminal_mut_by_id(&mut self, terminal_id: TerminalId) -> Option<&mut TerminalSession> {
        self.workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.terminals.iter_mut())
            .find(|terminal| terminal.id == terminal_id)
    }

    pub fn reset_terminal_statuses(&mut self) {
        for terminal in self
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.terminals.iter_mut())
        {
            terminal.status = TerminalStatus::Stopped;
        }
    }

    fn allocate_workspace_id(&mut self) -> WorkspaceId {
        let id = WorkspaceId(self.next_workspace_id);
        self.next_workspace_id += 1;
        id
    }

    fn allocate_chat_id(&mut self) -> ChatId {
        let id = ChatId(self.next_chat_id);
        self.next_chat_id += 1;
        id
    }

    fn allocate_terminal_id(&mut self) -> TerminalId {
        let id = TerminalId(self.next_terminal_id);
        self.next_terminal_id += 1;
        id
    }
}

impl ChatStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Thinking => "thinking",
            Self::Waiting => "waiting",
            Self::Done => "done",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Idle => Self::Thinking,
            Self::Thinking => Self::Waiting,
            Self::Waiting => Self::Done,
            Self::Done => Self::Idle,
        }
    }
}

impl TerminalLaunch {
    pub fn label(&self) -> String {
        match self {
            Self::Shell => "shell".to_string(),
            Self::Command(command) => command.clone(),
        }
    }
}

impl TerminalStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Stopped => Self::Running,
            Self::Running => Self::Stopped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_state_round_trips_through_json() {
        let state = ProjectState::default();

        let json = serde_json::to_string(&state).expect("serialize project state");
        let decoded: ProjectState = serde_json::from_str(&json).expect("deserialize project state");

        assert_eq!(decoded, state);
    }

    #[test]
    fn ids_continue_after_seed_data() {
        let mut state = ProjectState::default();
        let workspace = state.add_workspace("new".to_string(), None);
        let chat = state.add_chat(workspace, "agent".to_string(), ChatStatus::Idle);

        assert_eq!(workspace, WorkspaceId(3));
        assert_eq!(chat, Some(ChatId(4)));
    }

    #[test]
    fn terminal_launch_defaults_to_shell_for_old_state_files() {
        let json = r#"
        {
          "id": 1,
          "name": "shell",
          "status": "Stopped"
        }
        "#;

        let terminal: TerminalSession = serde_json::from_str(json).expect("deserialize terminal");

        assert_eq!(terminal.launch, TerminalLaunch::Shell);
    }
}
