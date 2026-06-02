use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;
pub const DEFAULT_AGENT_CHAT_TITLE: &str = "agent";
pub const RUNTIME_TERMINAL_ID_FLAG: u64 = 1 << 63;
pub const MAX_DURABLE_SESSION_ID: u64 = RUNTIME_TERMINAL_ID_FLAG - 1;

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
    #[serde(default)]
    pub agent: AgentKind,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
}

/// Which agent backend a chat drives. Persisted with the chat so a restored
/// session relaunches the same agent. Defaults to [`AgentKind::Pi`] so state
/// files written before agent selection existed keep their pi chats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentKind {
    #[default]
    Pi,
    ClaudeCode,
}

impl AgentKind {
    /// Compact tag shown next to a chat in the sidebar, e.g. the `pi` in
    /// `agent: pi`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::ClaudeCode => "cc",
        }
    }

    /// Human-readable name for command-palette entries and error messages.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::ClaudeCode => "Claude Code",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatMessageRole,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatMessageRole {
    User,
    Assistant,
    System,
    Tool,
    Error,
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

/// Identifies a PTY owned by the runtime: either a durable workspace terminal or
/// the agent process backing a chat. A runtime-only key (never persisted) that
/// replaces the old trick of stuffing a `ChatId` into a `TerminalId`'s high bit,
/// so the two cases are now distinct by type instead of a reused integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PtyKey {
    Terminal(TerminalId),
    ChatAgent(ChatId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatStatus {
    Idle,
    Thinking,
    Waiting,
    Failed,
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
        state.add_terminal(mult, "dev server".to_string(), TerminalStatus::Stopped);

        let website = state.add_workspace("website".to_string(), None);
        state.add_terminal(website, "shell".to_string(), TerminalStatus::Stopped);

        state
    }
}

#[cfg(test)]
impl ProjectState {
    /// Test fixture mirroring the historical first-run seed: the `mult`
    /// workspace with two agent chats and the `website` workspace with one.
    /// Startup no longer creates agent chats, so tests that need a populated
    /// project construct one explicitly via this helper.
    pub(crate) fn seeded() -> Self {
        let mut state = Self::default();
        let mult = state.workspaces[0].id;
        state.add_chat(
            mult,
            DEFAULT_AGENT_CHAT_TITLE.to_string(),
            ChatStatus::Idle,
            AgentKind::Pi,
        );
        state.add_chat(
            mult,
            DEFAULT_AGENT_CHAT_TITLE.to_string(),
            ChatStatus::Idle,
            AgentKind::Pi,
        );
        let website = state.workspaces[1].id;
        state.add_chat(
            website,
            DEFAULT_AGENT_CHAT_TITLE.to_string(),
            ChatStatus::Idle,
            AgentKind::Pi,
        );
        state
    }
}

impl ProjectState {
    pub fn normalize_next_ids(&mut self) -> bool {
        let mut changed = false;
        changed |= self.normalize_existing_ids();

        let required_workspace_id = next_unbounded_after(
            self.workspaces
                .iter()
                .map(|workspace| workspace.id.0)
                .collect(),
        );
        let required_chat_id = next_durable_after(
            self.workspaces
                .iter()
                .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id.0))
                .collect(),
        );
        let required_terminal_id = next_durable_after(
            self.workspaces
                .iter()
                .flat_map(|workspace| workspace.terminals.iter().map(|terminal| terminal.id.0))
                .collect(),
        );

        if self.next_workspace_id < required_workspace_id || self.next_workspace_id == 0 {
            self.next_workspace_id = required_workspace_id;
            changed = true;
        }
        if self.next_chat_id < required_chat_id || !is_valid_durable_session_id(self.next_chat_id) {
            self.next_chat_id = required_chat_id;
            changed = true;
        }
        if self.next_terminal_id < required_terminal_id
            || !is_valid_durable_session_id(self.next_terminal_id)
        {
            self.next_terminal_id = required_terminal_id;
            changed = true;
        }

        changed
    }

    fn normalize_existing_ids(&mut self) -> bool {
        let mut changed = false;
        let mut workspaces = BTreeSet::new();
        let mut next_workspace = 1;
        let mut chats = BTreeSet::new();
        let mut next_chat = 1;
        let mut terminals = BTreeSet::new();
        let mut next_terminal = 1;

        for workspace in &mut self.workspaces {
            if workspace.id.0 == 0 || !workspaces.insert(workspace.id.0) {
                workspace.id =
                    WorkspaceId(take_next_unbounded_id(&workspaces, &mut next_workspace));
                workspaces.insert(workspace.id.0);
                changed = true;
            }
            next_workspace = next_workspace.max(workspace.id.0.saturating_add(1));

            for chat in &mut workspace.chats {
                if !is_valid_durable_session_id(chat.id.0) || !chats.insert(chat.id.0) {
                    chat.id = ChatId(take_next_durable_id(&chats, &mut next_chat));
                    chats.insert(chat.id.0);
                    changed = true;
                }
                next_chat = next_chat.max(chat.id.0.saturating_add(1));
            }

            for terminal in &mut workspace.terminals {
                if !is_valid_durable_session_id(terminal.id.0) || !terminals.insert(terminal.id.0) {
                    terminal.id = TerminalId(take_next_durable_id(&terminals, &mut next_terminal));
                    terminals.insert(terminal.id.0);
                    changed = true;
                }
                next_terminal = next_terminal.max(terminal.id.0.saturating_add(1));
            }
        }

        changed
    }

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
        agent: AgentKind,
    ) -> Option<ChatId> {
        let workspace_index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)?;
        let id = self.allocate_chat_id();
        self.workspaces[workspace_index].chats.push(ChatSession {
            id,
            name,
            status,
            agent,
            messages: Vec::new(),
        });
        Some(id)
    }

    pub fn append_chat_message(
        &mut self,
        workspace_id: WorkspaceId,
        chat_id: ChatId,
        role: ChatMessageRole,
        text: String,
    ) -> bool {
        if text.is_empty() {
            return false;
        }

        let Some(chat) = self.chat_mut(workspace_id, chat_id) else {
            return false;
        };

        chat.messages.push(ChatMessage { role, text });
        true
    }

    pub fn append_chat_delta(
        &mut self,
        workspace_id: WorkspaceId,
        chat_id: ChatId,
        role: ChatMessageRole,
        text: &str,
    ) -> bool {
        if text.is_empty() {
            return false;
        }

        let Some(chat) = self.chat_mut(workspace_id, chat_id) else {
            return false;
        };

        if let Some(message) = chat
            .messages
            .last_mut()
            .filter(|message| message.role == role)
        {
            message.text.push_str(text);
        } else {
            chat.messages.push(ChatMessage {
                role,
                text: text.to_string(),
            });
        }

        true
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

    pub fn remove_workspace(&mut self, workspace_id: WorkspaceId) -> Option<Workspace> {
        let index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)?;
        Some(self.workspaces.remove(index))
    }

    pub fn remove_chat(
        &mut self,
        workspace_id: WorkspaceId,
        chat_id: ChatId,
    ) -> Option<ChatSession> {
        let workspace = self.workspace_mut(workspace_id)?;
        let index = workspace.chats.iter().position(|chat| chat.id == chat_id)?;
        Some(workspace.chats.remove(index))
    }

    pub fn remove_terminal(
        &mut self,
        workspace_id: WorkspaceId,
        terminal_id: TerminalId,
    ) -> Option<TerminalSession> {
        let workspace = self.workspace_mut(workspace_id)?;
        let index = workspace
            .terminals
            .iter()
            .position(|terminal| terminal.id == terminal_id)?;
        Some(workspace.terminals.remove(index))
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

    pub fn terminal_mut_by_id(&mut self, terminal_id: TerminalId) -> Option<&mut TerminalSession> {
        self.workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.terminals.iter_mut())
            .find(|terminal| terminal.id == terminal_id)
    }

    fn allocate_workspace_id(&mut self) -> WorkspaceId {
        let id = WorkspaceId(self.next_workspace_id);
        self.next_workspace_id += 1;
        id
    }

    fn allocate_chat_id(&mut self) -> ChatId {
        let id = ChatId(self.next_chat_id);
        self.next_chat_id = next_durable_candidate(self.next_chat_id.saturating_add(1));
        id
    }

    fn allocate_terminal_id(&mut self) -> TerminalId {
        let id = TerminalId(self.next_terminal_id);
        self.next_terminal_id = next_durable_candidate(self.next_terminal_id.saturating_add(1));
        id
    }
}

fn is_valid_durable_session_id(id: u64) -> bool {
    (1..=MAX_DURABLE_SESSION_ID).contains(&id)
}

fn next_durable_candidate(candidate: u64) -> u64 {
    if is_valid_durable_session_id(candidate) {
        candidate
    } else {
        1
    }
}

fn take_next_unbounded_id(used: &BTreeSet<u64>, next: &mut u64) -> u64 {
    while *next == 0 || used.contains(next) {
        *next = next.saturating_add(1).max(1);
    }
    let id = *next;
    *next = next.saturating_add(1);
    id
}

fn take_next_durable_id(used: &BTreeSet<u64>, next: &mut u64) -> u64 {
    *next = next_durable_candidate(*next);
    while used.contains(next) {
        *next = next_durable_candidate(next.saturating_add(1));
    }
    let id = *next;
    *next = next_durable_candidate(next.saturating_add(1));
    id
}

fn next_unbounded_after(used: BTreeSet<u64>) -> u64 {
    used.into_iter().max().unwrap_or(0).saturating_add(1).max(1)
}

fn next_durable_after(used: BTreeSet<u64>) -> u64 {
    next_durable_candidate(
        used.into_iter()
            .filter(|id| is_valid_durable_session_id(*id))
            .max()
            .unwrap_or(0)
            .saturating_add(1),
    )
}

impl ChatStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Thinking => "thinking",
            Self::Waiting => "waiting",
            Self::Failed => "failed",
            Self::Done => "done",
        }
    }
}

impl ChatMessageRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "agent",
            Self::System => "system",
            Self::Tool => "tool",
            Self::Error => "error",
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
        let mut state = ProjectState::seeded();
        let workspace = state.add_workspace("new".to_string(), None);
        let chat = state.add_chat(
            workspace,
            "agent".to_string(),
            ChatStatus::Idle,
            AgentKind::Pi,
        );

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

    #[test]
    fn chat_messages_default_for_old_state_files() {
        let json = r#"
        {
          "id": 1,
          "name": "agent",
          "status": "Idle"
        }
        "#;

        let chat: ChatSession = serde_json::from_str(json).expect("deserialize chat");

        assert!(chat.messages.is_empty());
        // State written before agent selection existed has no `agent` field and
        // must keep running pi.
        assert_eq!(chat.agent, AgentKind::Pi);
    }

    #[test]
    fn chat_deltas_append_to_last_message_with_same_role() {
        let mut state = ProjectState::seeded();
        let workspace = state.workspaces[0].id;
        let chat = state.workspaces[0].chats[0].id;

        assert!(state.append_chat_delta(workspace, chat, ChatMessageRole::Assistant, "hello"));
        assert!(state.append_chat_delta(workspace, chat, ChatMessageRole::Assistant, " world"));
        assert!(state.append_chat_delta(workspace, chat, ChatMessageRole::System, "done"));

        let messages = &state.chat(workspace, chat).unwrap().messages;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello world");
        assert_eq!(messages[1].role, ChatMessageRole::System);
    }

    #[test]
    fn normalize_next_ids_repairs_low_allocators() {
        let mut state = ProjectState {
            next_workspace_id: 1,
            next_chat_id: 1,
            next_terminal_id: 1,
            ..ProjectState::seeded()
        };

        assert!(state.normalize_next_ids());

        assert_eq!(state.next_workspace_id, 3);
        assert_eq!(state.next_chat_id, 4);
        assert_eq!(state.next_terminal_id, 3);
        assert_eq!(state.add_workspace("new".to_string(), None), WorkspaceId(3));
    }

    #[test]
    fn normalize_next_ids_keeps_higher_allocators() {
        let mut state = ProjectState {
            next_workspace_id: 99,
            next_chat_id: 99,
            next_terminal_id: 99,
            ..ProjectState::default()
        };

        assert!(!state.normalize_next_ids());

        assert_eq!(state.next_workspace_id, 99);
        assert_eq!(state.next_chat_id, 99);
        assert_eq!(state.next_terminal_id, 99);
    }

    #[test]
    fn normalize_next_ids_repairs_duplicate_and_reserved_session_ids() {
        let mut state = ProjectState::seeded();
        state.workspaces[0].chats[1].id = state.workspaces[0].chats[0].id;
        state.workspaces[1].chats[0].id = ChatId(RUNTIME_TERMINAL_ID_FLAG | 7);
        state.workspaces[1].terminals[0].id = state.workspaces[0].terminals[0].id;
        state.next_chat_id = RUNTIME_TERMINAL_ID_FLAG;
        state.next_terminal_id = RUNTIME_TERMINAL_ID_FLAG;

        assert!(state.normalize_next_ids());

        let chat_ids = state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id.0))
            .collect::<BTreeSet<_>>();
        assert_eq!(chat_ids.len(), 3);
        assert!(chat_ids.iter().all(|id| is_valid_durable_session_id(*id)));
        assert!(is_valid_durable_session_id(state.next_chat_id));

        let terminal_ids = state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter().map(|terminal| terminal.id.0))
            .collect::<BTreeSet<_>>();
        assert_eq!(terminal_ids.len(), 2);
        assert!(terminal_ids
            .iter()
            .all(|id| is_valid_durable_session_id(*id)));
        assert!(is_valid_durable_session_id(state.next_terminal_id));
    }

    #[test]
    fn remove_workspace_chat_and_terminal_by_id() {
        let mut state = ProjectState::seeded();
        let workspace = state.workspaces[0].id;
        let chat = state.workspaces[0].chats[0].id;
        let terminal = state.workspaces[0].terminals[0].id;

        assert_eq!(state.remove_chat(workspace, chat).unwrap().id, chat);
        assert!(state.chat(workspace, chat).is_none());

        assert_eq!(
            state.remove_terminal(workspace, terminal).unwrap().id,
            terminal
        );
        assert!(state.terminal(workspace, terminal).is_none());

        assert_eq!(state.remove_workspace(workspace).unwrap().id, workspace);
        assert!(state.workspace(workspace).is_none());
    }
}
