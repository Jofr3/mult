use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;
pub const DEFAULT_AGENT_CHAT_TITLE: &str = "agent";
pub const RUNTIME_TERMINAL_ID_FLAG: u64 = 1 << 63;
pub const MAX_DURABLE_SESSION_ID: u64 = RUNTIME_TERMINAL_ID_FLAG - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdAllocationError {
    Workspace,
    Chat,
    Terminal,
}

impl fmt::Display for IdAllocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let namespace = match self {
            Self::Workspace => "workspace",
            Self::Chat => "chat",
            Self::Terminal => "terminal",
        };
        write!(formatter, "{namespace} ID space is exhausted")
    }
}

impl std::error::Error for IdAllocationError {}

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

        let mult = state
            .add_workspace("mult".to_string(), std::env::current_dir().ok())
            .expect("initial workspace ID is available");
        let _ = state
            .add_terminal(mult, "dev server".to_string(), TerminalStatus::Stopped)
            .expect("initial terminal ID is available");

        let website = state
            .add_workspace("website".to_string(), None)
            .expect("initial workspace ID is available");
        let _ = state
            .add_terminal(website, "shell".to_string(), TerminalStatus::Stopped)
            .expect("initial terminal ID is available");

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
        let _ = state
            .add_chat(
                mult,
                DEFAULT_AGENT_CHAT_TITLE.to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("seed chat ID is available");
        let _ = state
            .add_chat(
                mult,
                DEFAULT_AGENT_CHAT_TITLE.to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("seed chat ID is available");
        let website = state.workspaces[1].id;
        let _ = state
            .add_chat(
                website,
                DEFAULT_AGENT_CHAT_TITLE.to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("seed chat ID is available");
        state
    }
}

impl ProjectState {
    pub fn normalize_next_ids(&mut self) -> Result<bool, IdAllocationError> {
        self.ensure_id_capacity()?;

        let mut changed = self.normalize_existing_ids()?;

        let workspace_ids = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.0)
            .collect::<BTreeSet<_>>();
        let chat_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id.0))
            .collect::<BTreeSet<_>>();
        let terminal_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter().map(|terminal| terminal.id.0))
            .collect::<BTreeSet<_>>();

        let next_workspace_id =
            normalized_allocator_hint(self.next_workspace_id, &workspace_ids, u64::MAX);
        let next_chat_id =
            normalized_allocator_hint(self.next_chat_id, &chat_ids, MAX_DURABLE_SESSION_ID);
        let next_terminal_id =
            normalized_allocator_hint(self.next_terminal_id, &terminal_ids, MAX_DURABLE_SESSION_ID);

        if self.next_workspace_id != next_workspace_id {
            self.next_workspace_id = next_workspace_id;
            changed = true;
        }
        if self.next_chat_id != next_chat_id {
            self.next_chat_id = next_chat_id;
            changed = true;
        }
        if self.next_terminal_id != next_terminal_id {
            self.next_terminal_id = next_terminal_id;
            changed = true;
        }

        Ok(changed)
    }

    fn ensure_id_capacity(&self) -> Result<(), IdAllocationError> {
        let chat_count = self.workspaces.iter().fold(0_u128, |count, workspace| {
            count.saturating_add(workspace.chats.len() as u128)
        });
        if chat_count > u128::from(MAX_DURABLE_SESSION_ID) {
            return Err(IdAllocationError::Chat);
        }

        let terminal_count = self.workspaces.iter().fold(0_u128, |count, workspace| {
            count.saturating_add(workspace.terminals.len() as u128)
        });
        if terminal_count > u128::from(MAX_DURABLE_SESSION_ID) {
            return Err(IdAllocationError::Terminal);
        }

        Ok(())
    }

    fn normalize_existing_ids(&mut self) -> Result<bool, IdAllocationError> {
        let mut changed = false;

        let mut workspace_ids = self
            .workspaces
            .iter()
            .filter_map(|workspace| (workspace.id.0 != 0).then_some(workspace.id.0))
            .collect::<BTreeSet<_>>();
        let mut seen_workspace_ids = BTreeSet::new();
        let mut next_workspace_id = 1;
        for workspace in &mut self.workspaces {
            if workspace.id.0 != 0 && seen_workspace_ids.insert(workspace.id.0) {
                continue;
            }

            let (id, next) = take_available_id(&mut workspace_ids, next_workspace_id, u64::MAX)
                .ok_or(IdAllocationError::Workspace)?;
            workspace.id = WorkspaceId(id);
            seen_workspace_ids.insert(id);
            next_workspace_id = next;
            changed = true;
        }

        let mut chat_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id.0))
            .filter(|id| is_valid_durable_session_id(*id))
            .collect::<BTreeSet<_>>();
        let mut seen_chat_ids = BTreeSet::new();
        let mut next_chat_id = 1;
        for chat in self
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.chats.iter_mut())
        {
            if is_valid_durable_session_id(chat.id.0) && seen_chat_ids.insert(chat.id.0) {
                continue;
            }

            let (id, next) = take_available_id(&mut chat_ids, next_chat_id, MAX_DURABLE_SESSION_ID)
                .ok_or(IdAllocationError::Chat)?;
            chat.id = ChatId(id);
            seen_chat_ids.insert(id);
            next_chat_id = next;
            changed = true;
        }

        let mut terminal_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter().map(|terminal| terminal.id.0))
            .filter(|id| is_valid_durable_session_id(*id))
            .collect::<BTreeSet<_>>();
        let mut seen_terminal_ids = BTreeSet::new();
        let mut next_terminal_id = 1;
        for terminal in self
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.terminals.iter_mut())
        {
            if is_valid_durable_session_id(terminal.id.0) && seen_terminal_ids.insert(terminal.id.0)
            {
                continue;
            }

            let (id, next) =
                take_available_id(&mut terminal_ids, next_terminal_id, MAX_DURABLE_SESSION_ID)
                    .ok_or(IdAllocationError::Terminal)?;
            terminal.id = TerminalId(id);
            seen_terminal_ids.insert(id);
            next_terminal_id = next;
            changed = true;
        }

        Ok(changed)
    }

    pub fn add_workspace(
        &mut self,
        name: String,
        cwd: Option<PathBuf>,
    ) -> Result<WorkspaceId, IdAllocationError> {
        let id = self.allocate_workspace_id()?;
        self.workspaces.push(Workspace {
            id,
            name,
            cwd,
            environment: BTreeMap::new(),
            chats: Vec::new(),
            terminals: Vec::new(),
        });
        Ok(id)
    }

    pub fn add_chat(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        status: ChatStatus,
        agent: AgentKind,
    ) -> Result<Option<ChatId>, IdAllocationError> {
        let Some(workspace_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
        else {
            return Ok(None);
        };
        let id = self.allocate_chat_id()?;
        self.workspaces[workspace_index].chats.push(ChatSession {
            id,
            name,
            status,
            agent,
            messages: Vec::new(),
        });
        Ok(Some(id))
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
    ) -> Result<Option<TerminalId>, IdAllocationError> {
        self.add_terminal_with_launch(workspace_id, name, status, TerminalLaunch::Shell)
    }

    pub fn add_command_terminal(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        status: TerminalStatus,
        command: String,
    ) -> Result<Option<TerminalId>, IdAllocationError> {
        self.add_terminal_with_launch(workspace_id, name, status, TerminalLaunch::Command(command))
    }

    fn add_terminal_with_launch(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        status: TerminalStatus,
        launch: TerminalLaunch,
    ) -> Result<Option<TerminalId>, IdAllocationError> {
        let Some(workspace_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
        else {
            return Ok(None);
        };
        let id = self.allocate_terminal_id()?;
        self.workspaces[workspace_index]
            .terminals
            .push(TerminalSession {
                id,
                name,
                status,
                launch,
            });
        Ok(Some(id))
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

    fn allocate_workspace_id(&mut self) -> Result<WorkspaceId, IdAllocationError> {
        let mut used = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.0)
            .collect::<BTreeSet<_>>();
        let (id, next) = take_available_id(&mut used, self.next_workspace_id, u64::MAX)
            .ok_or(IdAllocationError::Workspace)?;
        self.next_workspace_id = next;
        Ok(WorkspaceId(id))
    }

    fn allocate_chat_id(&mut self) -> Result<ChatId, IdAllocationError> {
        let mut used = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id.0))
            .filter(|id| is_valid_durable_session_id(*id))
            .collect::<BTreeSet<_>>();
        let (id, next) = take_available_id(&mut used, self.next_chat_id, MAX_DURABLE_SESSION_ID)
            .ok_or(IdAllocationError::Chat)?;
        self.next_chat_id = next;
        Ok(ChatId(id))
    }

    fn allocate_terminal_id(&mut self) -> Result<TerminalId, IdAllocationError> {
        let mut used = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter().map(|terminal| terminal.id.0))
            .filter(|id| is_valid_durable_session_id(*id))
            .collect::<BTreeSet<_>>();
        let (id, next) =
            take_available_id(&mut used, self.next_terminal_id, MAX_DURABLE_SESSION_ID)
                .ok_or(IdAllocationError::Terminal)?;
        self.next_terminal_id = next;
        Ok(TerminalId(id))
    }
}

fn is_valid_durable_session_id(id: u64) -> bool {
    (1..=MAX_DURABLE_SESSION_ID).contains(&id)
}

/// Takes a free ID at or after `hint`, wrapping once at `max`. The returned
/// next hint is zero only when taking this ID exhausted the supplied range.
fn take_available_id(used: &mut BTreeSet<u64>, hint: u64, max: u64) -> Option<(u64, u64)> {
    let id = find_available_id(used, hint, max)?;
    used.insert(id);
    let next = find_available_id(used, successor(id, max), max).unwrap_or(0);
    Some((id, next))
}

fn find_available_id(used: &BTreeSet<u64>, hint: u64, max: u64) -> Option<u64> {
    if max == 0 {
        return None;
    }

    let start = if (1..=max).contains(&hint) { hint } else { 1 };
    first_available_in_range(used, start, max).or_else(|| {
        if start > 1 {
            first_available_in_range(used, 1, start - 1)
        } else {
            None
        }
    })
}

fn first_available_in_range(used: &BTreeSet<u64>, start: u64, end: u64) -> Option<u64> {
    let mut candidate = start;
    for &id in used.range(start..=end) {
        if id > candidate {
            return Some(candidate);
        }
        if id == candidate {
            if candidate == end {
                return None;
            }
            candidate += 1;
        }
    }
    Some(candidate)
}

fn successor(id: u64, max: u64) -> u64 {
    if id == max {
        1
    } else {
        id + 1
    }
}

fn normalized_allocator_hint(current: u64, used: &BTreeSet<u64>, max: u64) -> u64 {
    let highest = used.range(1..=max).next_back().copied().unwrap_or(0);
    let start = if highest < max && (!(1..=max).contains(&current) || current <= highest) {
        highest + 1
    } else if (1..=max).contains(&current) {
        current
    } else {
        1
    };

    find_available_id(used, start, max).unwrap_or(0)
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
        let workspace = state
            .add_workspace("new".to_string(), None)
            .expect("workspace ID is available");
        let chat = state
            .add_chat(
                workspace,
                "agent".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("chat ID is available");

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

        assert!(state
            .normalize_next_ids()
            .expect("ID normalization should succeed"));

        assert_eq!(state.next_workspace_id, 3);
        assert_eq!(state.next_chat_id, 4);
        assert_eq!(state.next_terminal_id, 3);
        assert_eq!(
            state
                .add_workspace("new".to_string(), None)
                .expect("workspace ID is available"),
            WorkspaceId(3)
        );
    }

    #[test]
    fn normalize_next_ids_keeps_higher_allocators() {
        let mut state = ProjectState {
            next_workspace_id: 99,
            next_chat_id: 99,
            next_terminal_id: 99,
            ..ProjectState::default()
        };

        assert!(!state
            .normalize_next_ids()
            .expect("ID normalization should succeed"));

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

        assert!(state
            .normalize_next_ids()
            .expect("ID normalization should succeed"));

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
    fn workspace_allocation_wraps_safely_at_u64_max() {
        let mut state = empty_state();
        state.next_workspace_id = u64::MAX;

        let max = state
            .add_workspace("max".to_string(), None)
            .expect("u64::MAX is a valid workspace ID");
        let wrapped = state
            .add_workspace("wrapped".to_string(), None)
            .expect("allocation should wrap to a free ID");

        assert_eq!(max, WorkspaceId(u64::MAX));
        assert_eq!(wrapped, WorkspaceId(1));
        assert_eq!(state.next_workspace_id, 2);
    }

    #[test]
    fn durable_allocators_use_the_upper_bound_then_wrap() {
        let mut state = empty_state();
        let workspace = state
            .add_workspace("workspace".to_string(), None)
            .expect("workspace ID is available");
        state.next_chat_id = MAX_DURABLE_SESSION_ID;
        state.next_terminal_id = MAX_DURABLE_SESSION_ID;

        let max_chat = state
            .add_chat(
                workspace,
                "max chat".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("chat allocation should succeed");
        let max_terminal = state
            .add_terminal(
                workspace,
                "max terminal".to_string(),
                TerminalStatus::Stopped,
            )
            .expect("terminal allocation should succeed");
        let wrapped_chat = state
            .add_chat(
                workspace,
                "wrapped chat".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("chat allocation should wrap");
        let wrapped_terminal = state
            .add_terminal(
                workspace,
                "wrapped terminal".to_string(),
                TerminalStatus::Stopped,
            )
            .expect("terminal allocation should wrap");

        assert_eq!(max_chat, Some(ChatId(MAX_DURABLE_SESSION_ID)));
        assert_eq!(max_terminal, Some(TerminalId(MAX_DURABLE_SESSION_ID)));
        assert_eq!(wrapped_chat, Some(ChatId(1)));
        assert_eq!(wrapped_terminal, Some(TerminalId(1)));
        assert_eq!(state.next_chat_id, 2);
        assert_eq!(state.next_terminal_id, 2);
    }

    #[test]
    fn durable_normalization_wraps_at_the_upper_bound() {
        let mut state = empty_state();
        state.next_chat_id = MAX_DURABLE_SESSION_ID;
        state.next_terminal_id = MAX_DURABLE_SESSION_ID;
        let mut workspace = empty_workspace(WorkspaceId(1), "workspace");
        workspace.chats.push(ChatSession {
            id: ChatId(MAX_DURABLE_SESSION_ID),
            name: "chat".to_string(),
            status: ChatStatus::Idle,
            agent: AgentKind::Pi,
            messages: Vec::new(),
        });
        workspace.terminals.push(TerminalSession {
            id: TerminalId(MAX_DURABLE_SESSION_ID),
            name: "terminal".to_string(),
            status: TerminalStatus::Stopped,
            launch: TerminalLaunch::Shell,
        });
        state.workspaces.push(workspace);

        assert!(state
            .normalize_next_ids()
            .expect("normalization should wrap to a free durable ID"));

        assert_eq!(
            state.workspaces[0].chats[0].id,
            ChatId(MAX_DURABLE_SESSION_ID)
        );
        assert_eq!(
            state.workspaces[0].terminals[0].id,
            TerminalId(MAX_DURABLE_SESSION_ID)
        );
        assert_eq!(state.next_chat_id, 1);
        assert_eq!(state.next_terminal_id, 1);
    }

    #[test]
    fn allocation_skips_colliding_hints() {
        let mut state = ProjectState::default();
        state.next_workspace_id = state.workspaces[0].id.0;
        state.next_chat_id = 7;
        state.workspaces[0].chats.push(ChatSession {
            id: ChatId(7),
            name: "existing".to_string(),
            status: ChatStatus::Idle,
            agent: AgentKind::Pi,
            messages: Vec::new(),
        });

        let workspace = state
            .add_workspace("new".to_string(), None)
            .expect("workspace allocator should skip collisions");
        let chat = state
            .add_chat(
                workspace,
                "new".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("chat allocator should skip collisions");

        assert_eq!(workspace, WorkspaceId(3));
        assert_eq!(chat, Some(ChatId(8)));
    }

    #[test]
    fn normalization_repairs_duplicate_u64_max_ids_without_looping() {
        let mut state = empty_state();
        state.next_workspace_id = u64::MAX;
        state.workspaces = vec![
            empty_workspace(WorkspaceId(u64::MAX), "first"),
            empty_workspace(WorkspaceId(u64::MAX), "duplicate"),
        ];

        assert!(state
            .normalize_next_ids()
            .expect("normalization should find the wrapped gap"));

        assert_eq!(state.workspaces[0].id, WorkspaceId(u64::MAX));
        assert_eq!(state.workspaces[1].id, WorkspaceId(1));
        assert_eq!(state.next_workspace_id, 2);
    }

    #[test]
    fn bounded_allocator_reports_exhaustion_after_taking_last_id() {
        let mut used = BTreeSet::from([1, 2]);

        assert_eq!(take_available_id(&mut used, 3, 3), Some((3, 0)));
        assert_eq!(take_available_id(&mut used, 0, 3), None);
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

    fn empty_state() -> ProjectState {
        ProjectState {
            version: STATE_VERSION,
            next_workspace_id: 1,
            next_chat_id: 1,
            next_terminal_id: 1,
            workspaces: Vec::new(),
        }
    }

    fn empty_workspace(id: WorkspaceId, name: &str) -> Workspace {
        Workspace {
            id,
            name: name.to_string(),
            cwd: None,
            environment: BTreeMap::new(),
            chats: Vec::new(),
            terminals: Vec::new(),
        }
    }
}
