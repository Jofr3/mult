use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::File,
    io::{self, Read},
    path::PathBuf,
};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

pub const STATE_VERSION: u32 = 2;
pub const DEFAULT_AGENT_CHAT_TITLE: &str = "agent";
/// Upper bound on the text of a single persisted [`ChatMessage`].
///
/// Streamed assistant output arrives as deltas that are appended to the *last*
/// message, and nothing in the protocol terminates a message — so without a cap
/// one runaway agent grows one `String` without bound, and every save
/// re-serializes all of it. 64 KiB is roughly 10 000 words: far more than any
/// message a person reads in a chat pane, while keeping the whole transcript in
/// a range where `serde_json::to_string_pretty` stays inexpensive.
///
/// The transcript path this guards is Phase 3.4 scaffolding and currently has
/// no production caller (see `docs/ROADMAP.md` and the `agent` module), so this
/// is about being safe to wire up rather than a live bug being fixed.
pub const MAX_CHAT_MESSAGE_BYTES: usize = 64 * 1024;
/// Marks a message that hit [`MAX_CHAT_MESSAGE_BYTES`]. Written once, in place
/// of the bytes that were dropped, so a truncated transcript never looks like a
/// complete one.
pub const CHAT_MESSAGE_TRUNCATION_NOTICE: &str = "\n[truncated]";
pub const RUNTIME_TERMINAL_ID_FLAG: u64 = 1 << 63;
pub const MAX_DURABLE_SESSION_ID: u64 = RUNTIME_TERMINAL_ID_FLAG - 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdAllocationError {
    Workspace,
    Chat,
    Terminal,
    Identity(String),
}

impl fmt::Display for IdAllocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let namespace = match self {
            Self::Workspace => "workspace",
            Self::Chat => "chat",
            Self::Terminal => "terminal",
            Self::Identity(error) => {
                return write!(
                    formatter,
                    "could not allocate secure session identity: {error}"
                );
            }
        };
        write!(formatter, "{namespace} ID space is exhausted")
    }
}

impl std::error::Error for IdAllocationError {}

const IDENTITY_BYTES: usize = 16;
const MAX_IDENTITY_ATTEMPTS: usize = 64;

macro_rules! opaque_identity {
    ($name:ident, $description:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; IDENTITY_BYTES]);

        impl $name {
            pub fn from_bytes(bytes: [u8; IDENTITY_BYTES]) -> Option<Self> {
                (bytes != [0; IDENTITY_BYTES]).then_some(Self(bytes))
            }

            pub fn as_bytes(self) -> [u8; IDENTITY_BYTES] {
                self.0
            }

            fn from_random_bytes(bytes: [u8; IDENTITY_BYTES]) -> Option<Self> {
                Self::from_bytes(bytes)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let encoded = String::deserialize(deserializer)?;
                decode_identity(&encoded)
                    .and_then(Self::from_random_bytes)
                    .ok_or_else(|| {
                        de::Error::custom(concat!(
                            $description,
                            " must be 32 lowercase hexadecimal characters and not all zero"
                        ))
                    })
            }
        }
    };
}

opaque_identity!(StateNamespace, "state namespace");
opaque_identity!(SessionToken, "session token");
opaque_identity!(AgentGeneration, "agent generation");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub namespace: StateNamespace,
    pub token: SessionToken,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionIdentities {
    chats: BTreeMap<ChatId, SessionToken>,
    terminals: BTreeMap<TerminalId, SessionToken>,
}

impl SessionIdentities {
    fn chat(&self, id: ChatId) -> Option<SessionToken> {
        self.chats.get(&id).copied()
    }

    fn terminal(&self, id: TerminalId) -> Option<SessionToken> {
        self.terminals.get(&id).copied()
    }

    fn len(&self) -> usize {
        self.chats.len() + self.terminals.len()
    }

    fn is_empty(&self) -> bool {
        self.chats.is_empty() && self.terminals.is_empty()
    }
}

pub trait IdentitySource {
    fn fill_bytes(&mut self, bytes: &mut [u8]) -> io::Result<()>;
}

pub struct SecureIdentitySource(File);

impl SecureIdentitySource {
    pub fn new() -> io::Result<Self> {
        Ok(Self(File::open("/dev/urandom")?))
    }
}

impl IdentitySource for SecureIdentitySource {
    fn fill_bytes(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        self.0.read_exact(bytes)
    }
}

/// Decoding is deliberately lenient about fields that can be *reconstructed*
/// and strict about everything else.
///
/// A single renamed or `null`ed field used to make the whole file undecodable,
/// which routed it into backup-and-reset and discarded every workspace, chat
/// and terminal the user had (E11). Fields whose value can be rebuilt — the ID
/// allocator hints, the identity table, a workspace's terminals, a session's
/// status — therefore fall back to a default and are repaired on load, while
/// anything carrying information nothing else holds (an ID, a name, the state
/// namespace) stays required, because inventing one silently would be a
/// different kind of data loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectState {
    pub version: u32,
    pub(crate) namespace: StateNamespace,
    /// Reconciled against the sessions that decoded — see
    /// [`ProjectState::normalize_session_identities`].
    #[serde(default, deserialize_with = "null_or_default")]
    pub(crate) session_identities: SessionIdentities,
    /// Active daemon process incarnations for agent chats. Kept in a private
    /// identity table so ordinary chat edits cannot replace generations.
    #[serde(
        default,
        deserialize_with = "null_or_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub(crate) agent_generations: BTreeMap<ChatId, AgentGeneration>,
    /// Allocator hints, recomputed by `normalize_next_ids` when they are absent
    /// or inconsistent with the IDs actually in use.
    #[serde(default, deserialize_with = "null_or_default")]
    pub next_workspace_id: u64,
    #[serde(default, deserialize_with = "null_or_default")]
    pub next_chat_id: u64,
    #[serde(default, deserialize_with = "null_or_default")]
    pub next_terminal_id: u64,
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    #[serde(default, deserialize_with = "null_or_default")]
    pub cwd: Option<PathBuf>,
    #[serde(default, deserialize_with = "null_or_default")]
    pub environment: BTreeMap<String, String>,
    /// A workspace that loses its chats keeps its terminals, and vice versa:
    /// the two lists fail independently rather than taking the file with them.
    #[serde(default, deserialize_with = "null_or_default")]
    pub chats: Vec<ChatSession>,
    #[serde(default, deserialize_with = "null_or_default")]
    pub terminals: Vec<TerminalSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: ChatId,
    pub name: String,
    /// Runtime-derived: an unowned `Thinking`/`Waiting` is reset to `Idle` on
    /// load anyway, so a missing status costs nothing.
    #[serde(default, deserialize_with = "null_or_default")]
    pub status: ChatStatus,
    #[serde(default, deserialize_with = "null_or_default")]
    pub agent: AgentKind,
    #[serde(default, deserialize_with = "null_or_default")]
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
    /// Runtime-derived: the daemon is authoritative about whether a pane is
    /// live, so a missing status resolves itself on the first poll.
    #[serde(default, deserialize_with = "null_or_default")]
    pub status: TerminalStatus,
    #[serde(default, deserialize_with = "null_or_default")]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatStatus {
    #[default]
    Idle,
    Thinking,
    Waiting,
    Failed,
    Done,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalStatus {
    #[default]
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
        Self::try_default().expect("secure entropy is required to create project state")
    }
}

impl ProjectState {
    pub fn try_default() -> io::Result<Self> {
        let mut source = SecureIdentitySource::new()?;
        Self::try_default_with(&mut source)
    }

    pub fn try_default_with(source: &mut impl IdentitySource) -> io::Result<Self> {
        let mut state = Self {
            version: STATE_VERSION,
            namespace: generate_identity(source, |_| false)?,
            session_identities: SessionIdentities::default(),
            agent_generations: BTreeMap::new(),
            next_workspace_id: 1,
            next_chat_id: 1,
            next_terminal_id: 1,
            workspaces: Vec::new(),
        };

        let mult = state
            .add_workspace("mult".to_string(), std::env::current_dir().ok())
            .expect("initial workspace ID is available");
        state
            .add_terminal_with_source(
                mult,
                "dev server".to_string(),
                TerminalStatus::Stopped,
                TerminalLaunch::Shell,
                source,
            )
            .map_err(allocation_io_error)?;

        let website = state
            .add_workspace("website".to_string(), None)
            .expect("initial workspace ID is available");
        state
            .add_terminal_with_source(
                website,
                "shell".to_string(),
                TerminalStatus::Stopped,
                TerminalLaunch::Shell,
                source,
            )
            .map_err(allocation_io_error)?;

        Ok(state)
    }

    pub fn namespace(&self) -> StateNamespace {
        self.namespace
    }

    pub fn session_identity(&self, key: PtyKey) -> Option<SessionIdentity> {
        let token = match key {
            PtyKey::Terminal(id) => self.session_identities.terminal(id),
            PtyKey::ChatAgent(id) => self.session_identities.chat(id),
        }?;
        Some(SessionIdentity {
            namespace: self.namespace,
            token,
        })
    }

    pub fn validate_session_identities(&self) -> Result<(), &'static str> {
        let workspace_count = self.workspaces.len();
        let workspace_ids = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.0)
            .collect::<BTreeSet<_>>();
        if workspace_ids.len() != workspace_count || workspace_ids.contains(&0) {
            return Err("workspace IDs must be non-zero and unique");
        }

        let chat_count = self
            .workspaces
            .iter()
            .map(|workspace| workspace.chats.len())
            .sum::<usize>();
        let chat_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id))
            .collect::<BTreeSet<_>>();
        if chat_ids.len() != chat_count
            || chat_ids.iter().any(|id| !is_valid_durable_session_id(id.0))
        {
            return Err("chat IDs must be unique, non-zero durable IDs");
        }

        let terminal_count = self
            .workspaces
            .iter()
            .map(|workspace| workspace.terminals.len())
            .sum::<usize>();
        let terminal_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter().map(|terminal| terminal.id))
            .collect::<BTreeSet<_>>();
        if terminal_ids.len() != terminal_count
            || terminal_ids
                .iter()
                .any(|id| !is_valid_durable_session_id(id.0))
        {
            return Err("terminal IDs must be unique, non-zero durable IDs");
        }

        if chat_ids.len() != self.session_identities.chats.len()
            || chat_ids
                .iter()
                .any(|id| !self.session_identities.chats.contains_key(id))
        {
            return Err("chat session identity table does not match durable chats");
        }
        if terminal_ids.len() != self.session_identities.terminals.len()
            || terminal_ids
                .iter()
                .any(|id| !self.session_identities.terminals.contains_key(id))
        {
            return Err("terminal session identity table does not match durable terminals");
        }

        let unique_tokens = self
            .session_identities
            .chats
            .values()
            .chain(self.session_identities.terminals.values())
            .copied()
            .collect::<BTreeSet<_>>();
        if unique_tokens.len() != self.session_identities.len() {
            return Err("durable session tokens must be unique within a state namespace");
        }

        if self
            .agent_generations
            .keys()
            .any(|chat_id| !chat_ids.contains(chat_id))
        {
            return Err("active agent generation table references a missing chat");
        }
        let active_generations = self
            .agent_generations
            .values()
            .copied()
            .collect::<BTreeSet<_>>();
        if active_generations.len() != self.agent_generations.len() {
            return Err("active agent generations must be unique within durable state");
        }
        Ok(())
    }

    pub(crate) fn assign_session_identities(
        &mut self,
        source: &mut impl IdentitySource,
    ) -> io::Result<()> {
        if !self.session_identities.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session identity table must be empty before assignment",
            ));
        }

        let chat_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id))
            .collect::<Vec<_>>();
        for id in chat_ids {
            let token = self
                .allocate_session_token(source)
                .map_err(allocation_io_error)?;
            self.session_identities.chats.insert(id, token);
        }

        let terminal_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter().map(|terminal| terminal.id))
            .collect::<Vec<_>>();
        for id in terminal_ids {
            let token = self
                .allocate_session_token(source)
                .map_err(allocation_io_error)?;
            self.session_identities.terminals.insert(id, token);
        }
        Ok(())
    }

    /// Reconciles the identity and generation tables with the sessions that are
    /// actually present, returning whether anything changed.
    ///
    /// `validate_session_identities` demands an exact correspondence, so a
    /// state file that lost a chat — a renamed field, a `null` where an array
    /// belonged, a hand edit that added a terminal — failed startup outright
    /// even when everything else in it was intact. Reconciling keeps what
    /// decoded: entries for sessions that are gone are dropped, a duplicated
    /// token is re-minted, and a session with no token gets a fresh one.
    ///
    /// Minting is the fail-safe direction. A fresh token cannot claim the
    /// daemon session the old one owned, so the pane comes back stopped instead
    /// of adopting a session it cannot prove it owns (C12).
    pub(crate) fn normalize_session_identities(
        &mut self,
        source: &mut impl IdentitySource,
    ) -> io::Result<bool> {
        let chat_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id))
            .collect::<BTreeSet<_>>();
        let terminal_ids = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter().map(|terminal| terminal.id))
            .collect::<BTreeSet<_>>();

        let mut changed = false;
        let mut seen_tokens = BTreeSet::new();
        let identities = &mut self.session_identities;
        let before = identities.chats.len() + identities.terminals.len();
        identities
            .chats
            .retain(|id, token| chat_ids.contains(id) && seen_tokens.insert(*token));
        identities
            .terminals
            .retain(|id, token| terminal_ids.contains(id) && seen_tokens.insert(*token));
        changed |= identities.chats.len() + identities.terminals.len() != before;

        let generations = &mut self.agent_generations;
        let before = generations.len();
        let mut seen_generations = BTreeSet::new();
        generations
            .retain(|id, generation| chat_ids.contains(id) && seen_generations.insert(*generation));
        changed |= generations.len() != before;

        for id in chat_ids {
            if self.session_identities.chats.contains_key(&id) {
                continue;
            }
            let token = self
                .allocate_session_token(source)
                .map_err(allocation_io_error)?;
            self.session_identities.chats.insert(id, token);
            changed = true;
        }
        for id in terminal_ids {
            if self.session_identities.terminals.contains_key(&id) {
                continue;
            }
            let token = self
                .allocate_session_token(source)
                .map_err(allocation_io_error)?;
            self.session_identities.terminals.insert(id, token);
            changed = true;
        }

        Ok(changed)
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
        let mut source = SecureIdentitySource::new().map_err(identity_allocation_error)?;
        self.add_chat_with_source(workspace_id, name, status, agent, &mut source)
    }

    pub fn add_chat_with_source(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        status: ChatStatus,
        agent: AgentKind,
        source: &mut impl IdentitySource,
    ) -> Result<Option<ChatId>, IdAllocationError> {
        let Some(workspace_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
        else {
            return Ok(None);
        };
        let token = self.allocate_session_token(source)?;
        let id = self.allocate_chat_id()?;
        self.workspaces[workspace_index].chats.push(ChatSession {
            id,
            name,
            status,
            agent,
            messages: Vec::new(),
        });
        self.session_identities.chats.insert(id, token);
        Ok(Some(id))
    }

    pub fn active_agent_generation(&self, chat_id: ChatId) -> Option<AgentGeneration> {
        self.agent_generations.get(&chat_id).copied()
    }

    pub fn begin_agent_generation(
        &mut self,
        chat_id: ChatId,
    ) -> Result<Option<AgentGeneration>, IdAllocationError> {
        let mut source = SecureIdentitySource::new().map_err(identity_allocation_error)?;
        self.begin_agent_generation_with_source(chat_id, &mut source)
    }

    pub fn begin_agent_generation_with_source(
        &mut self,
        chat_id: ChatId,
        source: &mut impl IdentitySource,
    ) -> Result<Option<AgentGeneration>, IdAllocationError> {
        if !self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter())
            .any(|chat| chat.id == chat_id)
        {
            return Ok(None);
        }

        let generation = generate_identity(source, |candidate| {
            self.agent_generations
                .values()
                .any(|current| current == candidate)
        })
        .map_err(identity_allocation_error)?;
        self.agent_generations.insert(chat_id, generation);
        Ok(Some(generation))
    }

    pub fn clear_agent_generation(&mut self, chat_id: ChatId, generation: AgentGeneration) -> bool {
        if self.agent_generations.get(&chat_id) != Some(&generation) {
            return false;
        }
        self.agent_generations.remove(&chat_id);
        true
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

        let mut message = ChatMessage {
            role,
            text: String::new(),
        };
        push_capped_message_text(&mut message.text, &text);
        chat.messages.push(message);
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
            // A full message swallows further deltas *and* reports "unchanged",
            // so a runaway agent stops dirtying the project and therefore stops
            // provoking saves as well as stopping the growth.
            push_capped_message_text(&mut message.text, text)
        } else {
            let mut message = ChatMessage {
                role,
                text: String::new(),
            };
            push_capped_message_text(&mut message.text, text);
            chat.messages.push(message);
            true
        }
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
        let mut source = SecureIdentitySource::new().map_err(identity_allocation_error)?;
        self.add_terminal_with_source(workspace_id, name, status, launch, &mut source)
    }

    fn add_terminal_with_source(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        status: TerminalStatus,
        launch: TerminalLaunch,
        source: &mut impl IdentitySource,
    ) -> Result<Option<TerminalId>, IdAllocationError> {
        let Some(workspace_index) = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
        else {
            return Ok(None);
        };
        let token = self.allocate_session_token(source)?;
        let id = self.allocate_terminal_id()?;
        self.workspaces[workspace_index]
            .terminals
            .push(TerminalSession {
                id,
                name,
                status,
                launch,
            });
        self.session_identities.terminals.insert(id, token);
        Ok(Some(id))
    }

    pub fn remove_workspace(&mut self, workspace_id: WorkspaceId) -> Option<Workspace> {
        let index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)?;
        let removed = self.workspaces.remove(index);
        for chat in &removed.chats {
            self.session_identities.chats.remove(&chat.id);
            self.agent_generations.remove(&chat.id);
        }
        for terminal in &removed.terminals {
            self.session_identities.terminals.remove(&terminal.id);
        }
        Some(removed)
    }

    pub fn remove_chat(
        &mut self,
        workspace_id: WorkspaceId,
        chat_id: ChatId,
    ) -> Option<ChatSession> {
        let workspace = self.workspace_mut(workspace_id)?;
        let index = workspace.chats.iter().position(|chat| chat.id == chat_id)?;
        let removed = workspace.chats.remove(index);
        self.session_identities.chats.remove(&chat_id);
        self.agent_generations.remove(&chat_id);
        Some(removed)
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
        let removed = workspace.terminals.remove(index);
        self.session_identities.terminals.remove(&terminal_id);
        Some(removed)
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

    fn allocate_session_token(
        &self,
        source: &mut impl IdentitySource,
    ) -> Result<SessionToken, IdAllocationError> {
        generate_identity(source, |candidate| {
            self.session_identities
                .chats
                .values()
                .chain(self.session_identities.terminals.values())
                .any(|token| token == candidate)
        })
        .map_err(identity_allocation_error)
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

fn decode_identity(encoded: &str) -> Option<[u8; IDENTITY_BYTES]> {
    if encoded.len() != IDENTITY_BYTES * 2
        || encoded
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
    {
        return None;
    }

    let mut decoded = [0_u8; IDENTITY_BYTES];
    for (index, byte) in decoded.iter_mut().enumerate() {
        let high = decode_hex_digit(encoded.as_bytes()[index * 2])?;
        let low = decode_hex_digit(encoded.as_bytes()[index * 2 + 1])?;
        *byte = (high << 4) | low;
    }
    Some(decoded)
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Accepts a missing field, an explicit `null`, or a value.
///
/// `#[serde(default)]` alone covers only the *missing* case, and a `null` where
/// a struct or an array belongs is the other half of the shape errors that used
/// to cost a user their whole state file (E11). A wrong *type* is still an
/// error: `"workspaces": "nonsense"` carries data this code cannot interpret,
/// and quietly treating it as empty would destroy it rather than report it.
fn null_or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn generate_identity<T>(
    source: &mut impl IdentitySource,
    collision: impl Fn(&T) -> bool,
) -> io::Result<T>
where
    T: RandomIdentity,
{
    for _ in 0..MAX_IDENTITY_ATTEMPTS {
        let mut bytes = [0_u8; IDENTITY_BYTES];
        source.fill_bytes(&mut bytes)?;
        if let Some(identity) = T::from_random_bytes(bytes).filter(|value| !collision(value)) {
            return Ok(identity);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "secure entropy repeatedly produced zero or colliding identities",
    ))
}

trait RandomIdentity: Sized {
    fn from_random_bytes(bytes: [u8; IDENTITY_BYTES]) -> Option<Self>;
}

impl RandomIdentity for StateNamespace {
    fn from_random_bytes(bytes: [u8; IDENTITY_BYTES]) -> Option<Self> {
        StateNamespace::from_random_bytes(bytes)
    }
}

impl RandomIdentity for SessionToken {
    fn from_random_bytes(bytes: [u8; IDENTITY_BYTES]) -> Option<Self> {
        SessionToken::from_random_bytes(bytes)
    }
}

impl RandomIdentity for AgentGeneration {
    fn from_random_bytes(bytes: [u8; IDENTITY_BYTES]) -> Option<Self> {
        AgentGeneration::from_random_bytes(bytes)
    }
}

fn identity_allocation_error(error: io::Error) -> IdAllocationError {
    IdAllocationError::Identity(error.to_string())
}

fn allocation_io_error(error: IdAllocationError) -> io::Error {
    match error {
        IdAllocationError::Identity(message) => io::Error::other(message),
        other => io::Error::other(other.to_string()),
    }
}

fn is_valid_durable_session_id(id: u64) -> bool {
    (1..=MAX_DURABLE_SESSION_ID).contains(&id)
}

/// Append `text` to a chat message, enforcing [`MAX_CHAT_MESSAGE_BYTES`].
///
/// Returns whether anything was appended. The last
/// `CHAT_MESSAGE_TRUNCATION_NOTICE.len()` bytes of the budget are reserved for
/// the notice, so a message that overflows ends with it and lands exactly at or
/// above the content limit — which is what makes every later append a cheap,
/// allocation-free `false`. The notice is longer than the at most three bytes a
/// UTF-8 boundary retreat can cost, so that is guaranteed rather than likely.
fn push_capped_message_text(existing: &mut String, text: &str) -> bool {
    let content_limit = MAX_CHAT_MESSAGE_BYTES - CHAT_MESSAGE_TRUNCATION_NOTICE.len();
    let Some(budget) = content_limit.checked_sub(existing.len()) else {
        return false;
    };
    if budget == 0 {
        return false;
    }
    if text.len() <= budget {
        existing.push_str(text);
        return true;
    }

    // Never split a character: retreat to the nearest boundary at or below the
    // budget (at most three bytes).
    let mut keep = budget;
    while keep > 0 && !text.is_char_boundary(keep) {
        keep -= 1;
    }
    existing.push_str(&text[..keep]);
    existing.push_str(CHAT_MESSAGE_TRUNCATION_NOTICE);
    true
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
        let mut state = ProjectState::default();
        let workspace = state.workspaces[0].id;
        let chat = state
            .add_chat(
                workspace,
                "agent".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .unwrap()
            .unwrap();
        state.begin_agent_generation(chat).unwrap().unwrap();

        let json = serde_json::to_string(&state).expect("serialize project state");
        let decoded: ProjectState = serde_json::from_str(&json).expect("deserialize project state");

        assert_eq!(decoded, state);
    }

    /// B9: streamed deltas append to the last message forever, so the message
    /// itself carries the bound. Once it is reached the message stops growing
    /// *and* stops reporting a change, so it also stops provoking saves.
    #[test]
    fn a_streamed_message_stops_growing_at_the_cap() {
        let mut state = ProjectState::default();
        let workspace = state.workspaces[0].id;
        let chat = state
            .add_chat(
                workspace,
                "agent".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .unwrap()
            .unwrap();

        let delta = "x".repeat(8 * 1024);
        let mut appended = 0;
        for _ in 0..64 {
            if state.append_chat_delta(workspace, chat, ChatMessageRole::Assistant, &delta) {
                appended += 1;
            }
        }

        let messages = &state.chat(workspace, chat).unwrap().messages;
        assert_eq!(messages.len(), 1, "deltas keep extending one message");
        let text = &messages[0].text;
        assert!(
            text.len() <= MAX_CHAT_MESSAGE_BYTES,
            "message grew to {} bytes, past the {MAX_CHAT_MESSAGE_BYTES}-byte cap",
            text.len()
        );
        assert!(text.ends_with(CHAT_MESSAGE_TRUNCATION_NOTICE));
        assert!(
            appended < 64,
            "appends past the cap must report no change so they cannot dirty the project"
        );
        assert!(
            !state.append_chat_delta(workspace, chat, ChatMessageRole::Assistant, &delta),
            "a full message accepts nothing further"
        );
    }

    /// The truncation point is a byte budget, but it must still land on a
    /// character boundary — a split multi-byte character would make the whole
    /// state file undecodable, not just this message.
    #[test]
    fn truncation_never_splits_a_character() {
        let mut text = String::new();
        assert!(push_capped_message_text(
            &mut text,
            &"é".repeat(MAX_CHAT_MESSAGE_BYTES)
        ));

        assert!(text.ends_with(CHAT_MESSAGE_TRUNCATION_NOTICE));
        assert!(text.len() <= MAX_CHAT_MESSAGE_BYTES);
        let body = text
            .strip_suffix(CHAT_MESSAGE_TRUNCATION_NOTICE)
            .expect("notice is present");
        assert!(body.chars().all(|character| character == 'é'));
        assert!(!push_capped_message_text(&mut text, "more"));
    }

    /// A single oversized message (not a stream of deltas) is capped on the way
    /// in as well, so the cap cannot be bypassed by sending one huge message.
    #[test]
    fn a_single_oversized_message_is_capped_on_append() {
        let mut state = ProjectState::default();
        let workspace = state.workspaces[0].id;
        let chat = state
            .add_chat(
                workspace,
                "agent".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .unwrap()
            .unwrap();

        assert!(state.append_chat_message(
            workspace,
            chat,
            ChatMessageRole::Assistant,
            "y".repeat(MAX_CHAT_MESSAGE_BYTES * 4),
        ));

        let text = &state.chat(workspace, chat).unwrap().messages[0].text;
        assert!(text.len() <= MAX_CHAT_MESSAGE_BYTES);
        assert!(text.ends_with(CHAT_MESSAGE_TRUNCATION_NOTICE));
    }

    #[test]
    fn opaque_identities_reject_zero_and_noncanonical_encodings() {
        assert!(StateNamespace::from_bytes([0; 16]).is_none());
        assert!(
            serde_json::from_str::<StateNamespace>("\"00000000000000000000000000000000\"").is_err()
        );
        assert!(
            serde_json::from_str::<SessionToken>("\"ABCDEFABCDEFABCDEFABCDEFABCDEFAB\"").is_err()
        );
    }

    #[test]
    fn entropy_failure_does_not_partially_add_a_chat() {
        struct FailingSource;
        impl IdentitySource for FailingSource {
            fn fill_bytes(&mut self, _bytes: &mut [u8]) -> io::Result<()> {
                Err(io::Error::other("injected entropy failure"))
            }
        }

        let mut state = ProjectState::default();
        let workspace = state.workspaces[0].id;
        let before = state.clone();
        let error = state
            .add_chat_with_source(
                workspace,
                "agent".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
                &mut FailingSource,
            )
            .unwrap_err();

        assert!(error.to_string().contains("injected entropy failure"));
        assert_eq!(state, before);
    }

    #[test]
    fn agent_generations_are_nonzero_unique_and_cleared_only_by_exact_value() {
        let mut state = ProjectState::seeded();
        let first = state.workspaces[0].chats[0].id;
        let second = state.workspaces[0].chats[1].id;

        let first_generation = state.begin_agent_generation(first).unwrap().unwrap();
        let second_generation = state.begin_agent_generation(second).unwrap().unwrap();

        assert_ne!(first_generation, second_generation);
        assert_eq!(state.active_agent_generation(first), Some(first_generation));
        assert!(!state.clear_agent_generation(first, second_generation));
        assert!(state.clear_agent_generation(first, first_generation));
        assert_eq!(state.active_agent_generation(first), None);
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
        let mut state = ProjectState::default();
        state.workspaces.clear();
        state.session_identities = SessionIdentities::default();
        state.agent_generations.clear();
        ProjectState {
            version: STATE_VERSION,
            namespace: state.namespace,
            session_identities: state.session_identities,
            agent_generations: BTreeMap::new(),
            next_workspace_id: 1,
            next_chat_id: 1,
            next_terminal_id: 1,
            workspaces: Vec::new(),
        }
    }

    /// E11: reconciliation is what makes a lenient decode usable. Whatever the
    /// identity table lost, gained or duplicated, the state that comes out has
    /// to be one the writer accepts.
    #[test]
    fn reconciling_identities_drops_stale_entries_and_mints_missing_ones() {
        let mut source = SecureIdentitySource::new().unwrap();
        let mut state = ProjectState::default();
        let terminal = state.workspaces[0].terminals[0].id;
        let chat = state
            .add_chat(
                state.workspaces[0].id,
                "chat".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .unwrap()
            .unwrap();
        let generation = state.begin_agent_generation(chat).unwrap().unwrap();

        // A table describing a session that is gone, missing one that is here,
        // and reusing a token across the two.
        let stolen = state.session_identities.terminals[&terminal];
        state.session_identities.chats.insert(ChatId(9_999), stolen);
        state.session_identities.terminals.remove(&terminal);
        state.agent_generations.insert(ChatId(9_999), generation);

        assert!(state.normalize_session_identities(&mut source).unwrap());

        state.validate_session_identities().unwrap();
        assert!(!state.session_identities.chats.contains_key(&ChatId(9_999)));
        assert!(!state.agent_generations.contains_key(&ChatId(9_999)));
        assert!(state.session_identities.terminals.contains_key(&terminal));
        assert!(state.agent_generations.contains_key(&chat));
        // A consistent table is left exactly as it is.
        assert!(!state.normalize_session_identities(&mut source).unwrap());
    }

    /// E11: `#[serde(default)]` covers a missing field; a `null` needs the
    /// helper. Both must reach the same place, and a value of the wrong *type*
    /// must still be an error rather than being silently dropped.
    #[test]
    fn a_missing_or_null_reconstructible_field_decodes_to_its_default() {
        let missing: Workspace =
            serde_json::from_str(r#"{"id":1,"name":"w"}"#).expect("missing fields default");
        let nulled: Workspace = serde_json::from_str(
            r#"{"id":1,"name":"w","cwd":null,"environment":null,"chats":null,"terminals":null}"#,
        )
        .expect("null fields default");

        assert_eq!(missing, nulled);
        assert!(missing.chats.is_empty() && missing.terminals.is_empty());

        let terminal: TerminalSession =
            serde_json::from_str(r#"{"id":1,"name":"t"}"#).expect("missing status defaults");
        assert_eq!(terminal.status, TerminalStatus::Stopped);
        assert_eq!(terminal.launch, TerminalLaunch::Shell);

        // Wrong type, not absent: this carries data, and treating it as empty
        // would destroy it instead of reporting it.
        assert!(
            serde_json::from_str::<Workspace>(r#"{"id":1,"name":"w","chats":"none"}"#).is_err()
        );
        // Nothing reconstructs an ID or a name.
        assert!(serde_json::from_str::<Workspace>(r#"{"name":"w"}"#).is_err());
        assert!(serde_json::from_str::<Workspace>(r#"{"id":1}"#).is_err());
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
