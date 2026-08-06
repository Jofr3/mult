use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{BuildHasher, Hasher},
    io::Read,
    path::PathBuf,
};

use mult_protocol::{SessionId, SessionKind};
use serde::{Deserialize, Deserializer, Serialize};

pub use mult_protocol::{MAX_DURABLE_SESSION_ID, RUNTIME_SESSION_FLAG};

pub const STATE_VERSION: u32 = 1;
pub const DEFAULT_AGENT_CHAT_TITLE: &str = "agent";

/// Decoding is deliberately lenient, field by field (E11).
///
/// A `state.json` that fails to decode is moved aside and the session starts
/// empty, so every field that can carry a default rather than fail the whole
/// file does: one renamed key used to cost the user every workspace, chat and
/// terminal. [`null_as_default`] extends that to an explicit `null`, which
/// `#[serde(default)]` alone does not cover.
///
/// The leniency stops at whole elements: a workspace or chat that does not
/// decode at all still fails the load, because dropping it here would lose it
/// silently on the next save, whereas failing keeps the timestamped backup and
/// now says where it went.
///
/// It stops at the *containers* for the same reason (F6). `workspaces`, `chats`
/// and `terminals` are the sessions themselves, so a `null` or a renamed key
/// there is not a field that can carry a default — it is the whole project, and
/// defaulting it to an empty list hands the user a healthy-looking but empty
/// session, no backup, no notice, and a first save that overwrites the bytes
/// that still held everything. Those three fail the decode, which is what routes
/// the file to `backup_invalid_state_and_reset`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectState {
    #[serde(default, deserialize_with = "null_as_default")]
    pub version: u32,
    #[serde(default, deserialize_with = "null_as_default")]
    pub next_workspace_id: u64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub next_chat_id: u64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub next_terminal_id: u64,
    /// This project's daemon session namespace (A3), allocated on first use and
    /// then persisted. See [`ProjectState::ensure_instance`].
    #[serde(default, deserialize_with = "null_as_default")]
    pub instance: u64,
    /// Deliberately not lenient: see the note on this struct (F6).
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default, deserialize_with = "null_as_default")]
    pub id: WorkspaceId,
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub cwd: Option<PathBuf>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub environment: BTreeMap<String, String>,
    /// Deliberately not lenient: see the note on [`ProjectState`] (F6).
    pub chats: Vec<ChatSession>,
    /// Deliberately not lenient: see the note on [`ProjectState`] (F6).
    pub terminals: Vec<TerminalSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSession {
    #[serde(default, deserialize_with = "null_as_default")]
    pub id: ChatId,
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: ChatStatus,
    #[serde(default, deserialize_with = "null_as_default")]
    pub agent: AgentKind,
    /// Chat transcripts persisted by pre-11a state files.
    ///
    /// The transcript path they belong to was unreachable in production — the
    /// only writer was `AgentBackend::send_prompt`, which no production caller
    /// ever invoked — so it was deleted (F1). The field is still *read* so a
    /// state file that has one decodes intact rather than being moved aside,
    /// and never written back: [`ProjectState::take_legacy_chat_transcripts`]
    /// reports it to storage, which copies the pre-migration file aside before
    /// the first save drops it. Nothing else looks at the contents.
    #[serde(
        default,
        rename = "messages",
        skip_serializing,
        deserialize_with = "null_as_default"
    )]
    pub(crate) legacy_messages: Vec<LegacyChatMessage>,
}

/// One entry of a pre-11a persisted chat transcript. Decoded as leniently as
/// everything else in this file so its presence can never fail a load; only its
/// count is ever used. See [`ChatSession::legacy_messages`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LegacyChatMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub role: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub text: String,
}

/// A fresh, non-zero instance token.
///
/// Read from `/dev/urandom` when that is possible: the token namespaces this
/// client's sessions on a daemon that other processes of the same user can also
/// talk to, so an unguessable one keeps a process that has *not* read this
/// project's state file from naming its sessions at all (C12). The fallback —
/// the hasher's per-process random seed mixed with the clock and the pid — is
/// reliably *distinct*, which is what the collision problem (A3) needs, and is
/// only reached on a system with no `/dev/urandom`.
///
/// Zero is reserved for "no token", so a zero draw is nudged to one.
fn new_instance_token() -> u64 {
    let token = read_urandom_u64().unwrap_or_else(|| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0),
        );
        hasher.write_u32(std::process::id());
        hasher.finish()
    });
    token.max(1)
}

fn read_urandom_u64() -> Option<u64> {
    // `read_exact` of a fixed 8 bytes, never `read_to_end`: /dev/urandom never
    // reaches EOF.
    let mut bytes = [0_u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .ok()?;
    Some(u64::from_ne_bytes(bytes))
}

/// Decode `null` — and a missing key — as the field's default.
///
/// `#[serde(default)]` covers only the missing case: an explicit `null` where a
/// list, map or number is expected still fails the decode, and for `state.json`
/// a failed decode discards the whole project (E11).
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
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

    /// The config keys that decide how this agent is launched: its command line
    /// and its auto-start switch.
    ///
    /// The "not started" hint used to name pi's two keys for every kind, so a
    /// Claude Code chat was told to edit settings that have no effect on it
    /// (F18).
    pub fn config_keys(self) -> AgentConfigKeys {
        match self {
            Self::Pi => AgentConfigKeys {
                command: "pi_agent_command",
                auto_start: "auto_start_pi_agent",
            },
            Self::ClaudeCode => AgentConfigKeys {
                command: "claude_code_command",
                auto_start: "auto_start_claude_code_agent",
            },
        }
    }
}

/// The `config.json` keys belonging to one [`AgentKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentConfigKeys {
    pub command: &'static str,
    pub auto_start: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSession {
    #[serde(default, deserialize_with = "null_as_default")]
    pub id: TerminalId,
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: String,
    /// Whether the next launch should bring this terminal back.
    ///
    /// This is *intent*, not liveness. It used to be a persisted
    /// `TerminalStatus::{Running, Stopped}`, which meant the same question had
    /// two answers — the file and [`crate::pty::PtyRuntime`] — reconciled by
    /// hand from every site that started or stopped a PTY. Missing one left a
    /// terminal drawn as live forever, or auto-restarting a shell the user had
    /// closed. Displayed liveness now comes from the runtime alone, and this
    /// field only answers "replay it at startup?" (F16).
    ///
    /// Whether replaying actually *runs* anything without asking is a separate
    /// question, and still the launch's: a shell restores silently, a
    /// `Command` goes through the startup confirmation prompt (C1).
    #[serde(default, deserialize_with = "null_as_default")]
    pub restore_on_launch: bool,
    /// The pre-11a spelling of the field above. Read so a state file written by
    /// an older build still restores what it left running, and never written
    /// back — [`ProjectState::migrate_legacy_terminal_status`] folds it in once,
    /// at load.
    #[serde(
        default,
        rename = "status",
        skip_serializing,
        deserialize_with = "null_as_default"
    )]
    pub(crate) legacy_status: Option<TerminalStatus>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub launch: TerminalLaunch,
}

// Ids default to 0, which `normalize_existing_ids` treats as invalid and
// reallocates, so a session whose id did not decode is renumbered rather than
// lost.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct WorkspaceId(pub u64);

/// Generate a durable session id newtype whose value cannot leave
/// `0..=MAX_DURABLE_SESSION_ID` (F4).
///
/// The field is private and the only public constructor rejects the runtime
/// half of the id space, so [`PtyKey::wire_id`] can encode without a collision
/// being possible in the first place — the invariant used to be checked only
/// when a state file was loaded, and an id above the flag bit silently became a
/// session of the *other* kind on the wire.
///
/// `0` is the "not allocated" value: it is what [`Default`] and a state file
/// with a missing or unreadable id produce, and `normalize_existing_ids`
/// reallocates it. Decoding is deliberately lenient in the same way — an
/// out-of-range id in a state file becomes `0` and is renumbered, exactly as it
/// was before, rather than failing the whole load (E11).
macro_rules! durable_session_id {
    ($name:ident, $what:literal) => {
        #[doc = concat!("Identifies one persisted ", $what, ". See [`durable_session_id`].")]
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        pub struct $name(u64);

        impl $name {
            /// The id `value` names, or `None` when it is in the runtime half of
            /// the space and so cannot be a durable session.
            pub const fn new(value: u64) -> Option<Self> {
                if value > MAX_DURABLE_SESSION_ID {
                    None
                } else {
                    Some(Self(value))
                }
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Ok(Self::new(u64::deserialize(deserializer)?).unwrap_or_default())
            }
        }
    };
}

durable_session_id!(ChatId, "chat");
durable_session_id!(TerminalId, "terminal");

/// Identifies a PTY owned by the runtime: either a durable workspace terminal or
/// the agent process backing a chat. A runtime-only key (never persisted) that
/// replaces the old trick of stuffing a `ChatId` into a `TerminalId`'s high bit,
/// so the two cases are now distinct by type instead of a reused integer.
///
/// The daemon still sees one number per PTY — it has no notion of chats — but
/// that encoding now lives in [`mult_protocol::SessionId`], in both directions,
/// and is unreachable from an id that could collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PtyKey {
    Terminal(TerminalId),
    ChatAgent(ChatId),
}

impl PtyKey {
    /// The daemon-facing id for this PTY.
    ///
    /// `None` only for an unallocated (`0`) id, which no live session has: both
    /// id types already refuse the runtime half by construction, so the
    /// collision the old encoding allowed cannot be expressed here.
    pub fn wire_id(self) -> Option<SessionId> {
        match self {
            Self::Terminal(terminal) => SessionId::for_kind(SessionKind::Durable, terminal.get()),
            Self::ChatAgent(chat) => SessionId::for_kind(SessionKind::Runtime, chat.get()),
        }
    }

    /// The PTY a daemon-supplied session id names, or `None` when the id is
    /// malformed. A daemon that reports a pane id of `0` is reporting garbage;
    /// the old inverse read it as terminal `0` and handed it to the client as a
    /// real terminal.
    pub fn from_wire_id(session: SessionId) -> Option<Self> {
        let (kind, id) = session.split()?;
        match kind {
            SessionKind::Durable => TerminalId::new(id).map(Self::Terminal),
            SessionKind::Runtime => ChatId::new(id).map(Self::ChatAgent),
        }
    }
}

/// What a chat's agent is doing, plus — for a finished one — whether the user
/// has looked at it yet.
///
/// The seen bit used to be a `BTreeSet<ChatId>` beside the status, kept in step
/// by a `reconcile_done_seen` call at every site that changed one. It is a fact
/// *about* `Done` and about nothing else, so it lives in `Done` (F16); a state
/// it does not qualify cannot carry it, and there is nothing left to reconcile.
///
/// It is deliberately not persisted: see [`PersistedChatStatus`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "PersistedChatStatus", into = "PersistedChatStatus")]
pub enum ChatStatus {
    #[default]
    Idle,
    Thinking,
    Waiting,
    Failed,
    /// Finished. `seen` is false while the sidebar should show the green
    /// "finished" notification, and true once the user has been looking at the
    /// chat while it was in this state.
    Done {
        seen: bool,
    },
}

/// The on-disk spelling of [`ChatStatus`] — the same five bare strings as
/// before 11a, so no state file changes shape and none needs migrating.
///
/// The seen bit is about *this* session's notifications, not about the chat, so
/// it does not survive a restart: a finished chat is unseen again on the next
/// launch, which is exactly what the old runtime-only `seen_done` set produced
/// by starting empty.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersistedChatStatus {
    #[default]
    Idle,
    Thinking,
    Waiting,
    Failed,
    Done,
}

impl From<PersistedChatStatus> for ChatStatus {
    fn from(status: PersistedChatStatus) -> Self {
        match status {
            PersistedChatStatus::Idle => Self::Idle,
            PersistedChatStatus::Thinking => Self::Thinking,
            PersistedChatStatus::Waiting => Self::Waiting,
            PersistedChatStatus::Failed => Self::Failed,
            PersistedChatStatus::Done => Self::Done { seen: false },
        }
    }
}

impl From<ChatStatus> for PersistedChatStatus {
    fn from(status: ChatStatus) -> Self {
        match status {
            ChatStatus::Idle => Self::Idle,
            ChatStatus::Thinking => Self::Thinking,
            ChatStatus::Waiting => Self::Waiting,
            ChatStatus::Failed => Self::Failed,
            ChatStatus::Done { .. } => Self::Done,
        }
    }
}

/// The pre-11a persisted terminal status, kept only so an older `state.json`
/// still says which terminals to bring back. `Running` becomes
/// `restore_on_launch: true`; nothing writes this type any more.
///
/// Defaults to `Stopped`: a terminal whose status did not decode is not known
/// to have been running, and restoring one that was not is worse than not
/// restoring one that was.
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

/// An empty project: no workspaces, and nothing read from the environment.
///
/// It used to seed two hardcoded demo workspaces — a `mult` one from
/// `current_dir()` and a `website` one with a `shell` terminal — which was also
/// what storage handed back after moving a corrupt state file aside. A user
/// whose state was damaged was therefore shown a fabricated `website` workspace
/// and told, in the same breath, that the session was "starting empty" (F12).
/// The seed is now [`ProjectState::first_run_seed`], reached only when no state
/// file existed at all.
impl Default for ProjectState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            next_workspace_id: 1,
            next_chat_id: 1,
            next_terminal_id: 1,
            // Deliberately unset: allocating here would make two default states
            // unequal, and the token is only needed once something connects.
            instance: 0,
            workspaces: Vec::new(),
        }
    }
}

impl ProjectState {
    /// What a brand-new installation starts with: a workspace named after the
    /// directory `mult` was launched from, and a stopped terminal in it.
    ///
    /// `cwd` is a parameter rather than an `env::current_dir()` call so that
    /// constructing a project never depends on where the process happens to be.
    pub fn first_run_seed(cwd: Option<PathBuf>) -> Self {
        let mut state = Self::default();
        let name = cwd
            .as_deref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".to_string());
        let workspace = state.add_workspace(name, cwd);
        state.add_terminal(workspace, "shell".to_string(), false);
        state
    }
}

#[cfg(test)]
impl ProjectState {
    /// Test fixture: two workspaces with one terminal each, which is the shape
    /// `Default` used to hand every user (F12). Kept — under a name that says
    /// it is a fixture — because a good deal of navigation, sidebar and
    /// deletion behaviour is only interesting with more than one workspace.
    pub(crate) fn two_workspaces() -> Self {
        let mut state = Self::default();
        let mult = state.add_workspace("mult".to_string(), std::env::current_dir().ok());
        state.add_terminal(mult, "dev server".to_string(), false);
        let website = state.add_workspace("website".to_string(), None);
        state.add_terminal(website, "shell".to_string(), false);
        state
    }

    /// [`ProjectState::two_workspaces`] plus three agent chats, for tests that
    /// exercise chat behaviour. Startup creates no agent chats at all.
    pub(crate) fn seeded() -> Self {
        let mut state = Self::two_workspaces();
        let mult = state.workspaces[0].id;
        let website_workspace = state.workspaces[1].id;
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
        state.add_chat(
            website_workspace,
            DEFAULT_AGENT_CHAT_TITLE.to_string(),
            ChatStatus::Idle,
            AgentKind::Pi,
        );
        state
    }
}

impl ProjectState {
    /// This project's daemon session namespace, allocating and persisting one on
    /// first use (A3).
    ///
    /// Session ids on the wire are this project's own `TerminalId`s, which say
    /// nothing about *whose* they are: without a namespace a second `mult` —
    /// another window, another state file, another user's copy of one — asks for
    /// session 1 and is handed the first instance's shell, then evicts its
    /// owner. The token is stored with the project so a restarted client
    /// reclaims exactly the panes it left behind, which is the entire point of
    /// having a daemon.
    ///
    /// Returns whether a token had to be allocated, so the caller can persist it.
    pub fn ensure_instance(&mut self) -> bool {
        if self.instance != 0 {
            return false;
        }
        self.instance = new_instance_token();
        true
    }

    /// Drop every pre-11a chat transcript this state carried in, returning how
    /// many messages went with them.
    ///
    /// Called once by storage immediately after decoding. A non-zero answer is
    /// the signal that the file on disk holds data this build no longer keeps,
    /// so storage copies the file aside and says where before the first save
    /// rewrites it without them (F1).
    pub fn take_legacy_chat_transcripts(&mut self) -> usize {
        self.workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.chats.iter_mut())
            .map(|chat| std::mem::take(&mut chat.legacy_messages).len())
            .sum()
    }

    /// Fold a pre-11a `status` field into `restore_on_launch`.
    ///
    /// Called once by storage after decoding, before anything can save over the
    /// file. `Running` meant "this was live when we last exited", which is
    /// precisely the intent the new field records, so a state file written by an
    /// older build restores exactly what it used to. The legacy field is cleared
    /// so the migration cannot run twice, and it is never serialized.
    pub fn migrate_legacy_terminal_status(&mut self) -> bool {
        let mut migrated = false;
        for terminal in self
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.terminals.iter_mut())
        {
            if let Some(status) = terminal.legacy_status.take() {
                terminal.restore_on_launch |= status == TerminalStatus::Running;
                migrated = true;
            }
        }
        migrated
    }

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
            legacy_messages: Vec::new(),
        });
        Some(id)
    }

    pub fn add_terminal(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        restore_on_launch: bool,
    ) -> Option<TerminalId> {
        self.add_terminal_with_launch(workspace_id, name, restore_on_launch, TerminalLaunch::Shell)
    }

    pub fn add_command_terminal(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        restore_on_launch: bool,
        command: String,
    ) -> Option<TerminalId> {
        self.add_terminal_with_launch(
            workspace_id,
            name,
            restore_on_launch,
            TerminalLaunch::Command(command),
        )
    }

    fn add_terminal_with_launch(
        &mut self,
        workspace_id: WorkspaceId,
        name: String,
        restore_on_launch: bool,
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
                restore_on_launch,
                legacy_status: None,
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

    /// Allocate an unused workspace id.
    ///
    /// Workspace ids are not bounded by the session-id space, so the counter
    /// only wraps at `u64::MAX` — but a state file that already carries that id
    /// saturates `next_unbounded_after`, and the bare `WorkspaceId(next)` this
    /// replaced then handed out a *duplicate*: `workspace_mut` resolved to the
    /// wrong entry and `remove_workspace` deleted the wrong one. The `+= 1` that
    /// followed it overflowed on top (a panic in debug, the reserved `0` in
    /// release). Consulting the used ids is what `allocate_chat_id` and
    /// `allocate_terminal_id` already do (F5).
    fn allocate_workspace_id(&mut self) -> WorkspaceId {
        let used = self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.0)
            .collect::<BTreeSet<_>>();
        WorkspaceId(take_next_unbounded_id(&used, &mut self.next_workspace_id))
    }

    /// Allocate an unused chat id. The counter wraps back to 1 at
    /// [`MAX_DURABLE_SESSION_ID`], and a wrapped (or hand-edited) counter can
    /// land on an id a live chat already holds, so the used ids are consulted —
    /// exactly as `normalize_next_ids` does when repairing a state file.
    fn allocate_chat_id(&mut self) -> ChatId {
        let used = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id.0))
            .collect::<BTreeSet<_>>();
        ChatId(take_next_durable_id(&used, &mut self.next_chat_id))
    }

    /// Allocate an unused terminal id; see [`Self::allocate_chat_id`].
    fn allocate_terminal_id(&mut self) -> TerminalId {
        let used = self
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.terminals.iter().map(|terminal| terminal.id.0))
            .collect::<BTreeSet<_>>();
        TerminalId(take_next_durable_id(&used, &mut self.next_terminal_id))
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

/// Take the next unused id from an unbounded counter, wrapping past the end.
///
/// `wrapping_add`, never `saturating_add`: at `u64::MAX` a saturating bump is a
/// no-op, so a counter parked there with that id already in use spun here
/// forever. `normalize_existing_ids` reaches exactly that state from a state
/// file with two workspaces both carrying `"id": 18446744073709551615`, and
/// `App::new` runs it before the first frame — so `mult` hung at 100% CPU on
/// every launch, with nothing on screen and no key to press (F4). The wrap
/// terminates because `used` is finite; `0` is reserved for "unallocated" and is
/// skipped on the way past.
fn take_next_unbounded_id(used: &BTreeSet<u64>, next: &mut u64) -> u64 {
    while *next == 0 || used.contains(next) {
        *next = next.wrapping_add(1);
    }
    let id = *next;
    *next = next.wrapping_add(1);
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
    /// Whether two statuses describe the same state, ignoring the presentation
    /// bit `Done` carries. `Done { seen: true }` and `Done { seen: false }` are
    /// the same state: the agent finished either way.
    pub fn is_same_state(self, other: Self) -> bool {
        PersistedChatStatus::from(self) == PersistedChatStatus::from(other)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Thinking => "thinking",
            Self::Waiting => "waiting",
            Self::Failed => "failed",
            Self::Done { .. } => "done",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
    fn the_starter_seed_is_one_workspace_named_after_the_launch_directory() {
        // And `Default` is empty, so nothing that merely wants a blank project
        // — including corrupt-state recovery — invents workspaces (F12).
        assert!(ProjectState::default().workspaces.is_empty());

        let seed = ProjectState::first_run_seed(Some(PathBuf::from("/home/dev/orbit")));

        assert_eq!(seed.workspaces.len(), 1);
        assert_eq!(seed.workspaces[0].name, "orbit");
        assert_eq!(
            seed.workspaces[0].cwd.as_deref(),
            Some(Path::new("/home/dev/orbit"))
        );
        assert_eq!(seed.workspaces[0].terminals.len(), 1);
        assert!(seed.workspaces[0].chats.is_empty());
        // No usable directory still yields something to work in.
        assert_eq!(ProjectState::first_run_seed(None).workspaces.len(), 1);
    }

    #[test]
    fn a_chat_id_can_never_reach_the_runtime_half_of_the_session_space() {
        // The invariant used to be enforced only when a state file was loaded,
        // so a `ChatId` at or above the flag bit encoded as a *terminal* on the
        // wire and the client happily treated the daemon's reply as one (F4).
        assert_eq!(ChatId::new(RUNTIME_SESSION_FLAG), None);
        assert_eq!(ChatId::new(u64::MAX), None);
        assert_eq!(TerminalId::new(RUNTIME_SESSION_FLAG), None);
        assert_eq!(
            TerminalId::new(MAX_DURABLE_SESSION_ID).map(TerminalId::get),
            Some(MAX_DURABLE_SESSION_ID)
        );

        // The two halves stay distinct all the way to the wire and back.
        let chat = PtyKey::ChatAgent(ChatId::new(7).expect("chat id"));
        let terminal = PtyKey::Terminal(TerminalId::new(7).expect("terminal id"));
        assert_ne!(chat.wire_id(), terminal.wire_id());
        assert_eq!(PtyKey::from_wire_id(chat.wire_id().unwrap()), Some(chat));
        assert_eq!(
            PtyKey::from_wire_id(terminal.wire_id().unwrap()),
            Some(terminal)
        );
        // A daemon naming pane 0 is naming nothing, not terminal 0.
        assert_eq!(PtyKey::from_wire_id(SessionId(0)), None);
    }

    #[test]
    fn a_state_file_id_in_the_runtime_half_decodes_as_unallocated() {
        // Leniently, not fatally: `normalize_existing_ids` renumbers it, which
        // is exactly what happened before the range was enforced by the type.
        let json = format!(r#"{{"id":{RUNTIME_SESSION_FLAG},"name":"agent"}}"#);

        let chat: ChatSession = serde_json::from_str(&json).expect("deserialize chat");

        assert_eq!(chat.id, ChatId::default());
        assert!(!is_valid_durable_session_id(chat.id.get()));
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
    fn chat_fields_default_for_old_state_files() {
        let json = r#"
        {
          "id": 1,
          "name": "agent",
          "status": "Idle"
        }
        "#;

        let chat: ChatSession = serde_json::from_str(json).expect("deserialize chat");

        // State written before agent selection existed has no `agent` field and
        // must keep running pi.
        assert_eq!(chat.agent, AgentKind::Pi);
        assert!(chat.legacy_messages.is_empty());
    }

    /// F1: a pre-11a chat carrying a transcript still decodes, the transcript is
    /// handed to storage exactly once, and it is never serialized back.
    #[test]
    fn a_pre_11a_chat_transcript_decodes_and_is_taken_once() {
        let json = r#"
        {
          "version": 1,
          "next_workspace_id": 2,
          "next_chat_id": 2,
          "next_terminal_id": 1,
          "workspaces": [
            {
              "id": 1,
              "name": "orbit",
              "chats": [
                {
                  "id": 1,
                  "name": "agent",
                  "status": "Done",
                  "messages": [
                    { "role": "User", "text": "hello" },
                    { "role": "Assistant", "text": "hi" }
                  ]
                }
              ],
              "terminals": []
            }
          ]
        }
        "#;

        let mut state: ProjectState = serde_json::from_str(json).expect("deserialize state");

        // Everything else survived the field the build no longer keeps.
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].name, "orbit");
        assert_eq!(state.workspaces[0].chats[0].name, "agent");
        assert_eq!(state.take_legacy_chat_transcripts(), 2);
        assert_eq!(state.take_legacy_chat_transcripts(), 0, "taken once");

        let reserialized = serde_json::to_string(&state).expect("serialize state");
        assert!(
            !reserialized.contains("messages"),
            "the dropped field must not be written back: {reserialized}"
        );
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
        state.workspaces[1].chats[0].id = ChatId(RUNTIME_SESSION_FLAG | 7);
        state.workspaces[1].terminals[0].id = state.workspaces[0].terminals[0].id;
        state.next_chat_id = RUNTIME_SESSION_FLAG;
        state.next_terminal_id = RUNTIME_SESSION_FLAG;

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

    #[test]
    fn wrapped_id_counter_does_not_hand_out_a_duplicate_chat_id() {
        // Drives the wrap branch: the counter is one short of the maximum, so
        // the allocation after it wraps to 1 — which a live chat already holds
        // (reachable from a hand-edited or corrupt state file).
        let mut state = ProjectState::two_workspaces();
        let workspace = state.workspaces[0].id;
        state.next_chat_id = 1;
        let first = state
            .add_chat(
                workspace,
                "one".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("add first chat");
        assert_eq!(first, ChatId(1));
        state.next_chat_id = MAX_DURABLE_SESSION_ID;

        let last = state
            .add_chat(
                workspace,
                "last".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("add last chat");
        let wrapped = state
            .add_chat(
                workspace,
                "wrapped".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("add wrapped chat");

        assert_eq!(last, ChatId(MAX_DURABLE_SESSION_ID));
        assert_ne!(wrapped, first);
        assert_eq!(wrapped, ChatId(2));
    }

    /// F4: `mult` hung at 100% CPU on every launch, before the first frame, for
    /// any state file carrying two workspaces with `"id": 18446744073709551615`
    /// — `saturating_add(1).max(1)` at `u64::MAX` is a no-op, so the search for
    /// a free id never advanced. `WorkspaceId` accepts the value, `App::new`
    /// runs the repair, and there was no key to press.
    #[test]
    fn two_maximal_workspace_ids_are_renumbered_instead_of_spinning_forever() {
        let json = format!(
            r#"{{"version":1,"next_workspace_id":{max},"next_chat_id":1,"next_terminal_id":1,
                 "workspaces":[
                   {{"id":{max},"name":"one","chats":[],"terminals":[]}},
                   {{"id":{max},"name":"two","chats":[],"terminals":[]}}]}}"#,
            max = u64::MAX
        );
        let mut state: ProjectState = serde_json::from_str(&json).expect("deserialize state");

        assert!(state.normalize_next_ids());

        let ids = state
            .workspaces
            .iter()
            .map(|workspace| workspace.id.0)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), 2, "a duplicate id has to be renumbered");
        assert!(!ids.contains(&0), "0 is the unallocated id");
    }

    /// F5: `allocate_workspace_id` handed out `next_workspace_id` unchecked, so
    /// an existing workspace at `u64::MAX` (where the counter saturates) made
    /// the next workspace a *duplicate* of it — `workspace_mut` then resolved to
    /// the wrong entry and `remove_workspace` deleted the wrong one — and the
    /// `+= 1` behind it overflowed: a panic in debug, the reserved `0` in
    /// release.
    #[test]
    fn a_maximal_workspace_id_yields_neither_a_duplicate_nor_an_overflow() {
        let mut state = ProjectState::default();
        let existing = WorkspaceId(u64::MAX);
        state.workspaces.push(Workspace {
            id: existing,
            name: "existing".to_string(),
            cwd: None,
            environment: BTreeMap::new(),
            chats: Vec::new(),
            terminals: Vec::new(),
        });
        state.next_workspace_id = u64::MAX;

        let added = state.add_workspace("new".to_string(), None);
        let after = state.add_workspace("newer".to_string(), None);

        assert_ne!(added, existing);
        assert_ne!(added, WorkspaceId(0));
        assert_ne!(after, added);
        assert_eq!(
            state
                .workspaces
                .iter()
                .map(|workspace| workspace.id)
                .collect::<BTreeSet<_>>()
                .len(),
            3
        );
        // The entry each id names is still the one it was given to.
        assert_eq!(state.workspace(added).map(|w| w.name.as_str()), Some("new"));
        assert_eq!(
            state.remove_workspace(existing).map(|w| w.name),
            Some("existing".to_string())
        );
    }

    /// F6: the sessions themselves are not a defaultable field. A `null` (or a
    /// renamed key) where they belong is damage, and decoding it as an empty
    /// list hands the user a healthy-looking workspace with everything gone, no
    /// backup and no notice — see the storage tests for the other half.
    #[test]
    fn a_null_or_missing_session_container_fails_the_decode() {
        let head = r#""version":1,"next_workspace_id":1,"next_chat_id":1,"next_terminal_id":1"#;
        for state in [
            format!(r#"{{{head},"workspaces":null}}"#),
            format!(r#"{{{head}}}"#),
            format!(
                r#"{{{head},"workspaces":[{{"id":1,"name":"orbit","chats":null,"terminals":[]}}]}}"#
            ),
            format!(
                r#"{{{head},"workspaces":[{{"id":1,"name":"orbit","chats":[],"terminals":null}}]}}"#
            ),
            format!(r#"{{{head},"workspaces":[{{"id":1,"name":"orbit","chats":[]}}]}}"#),
        ] {
            assert!(
                serde_json::from_str::<ProjectState>(&state).is_err(),
                "must not decode: {state}"
            );
        }

        // The scalars around them stay lenient: that is what E11 was about.
        let lenient = r#"{"version":null,"next_workspace_id":null,"next_chat_id":null,
             "next_terminal_id":null,
             "workspaces":[{"id":null,"name":null,"cwd":null,"environment":null,
               "chats":[],"terminals":[]}]}"#;
        let state: ProjectState = serde_json::from_str(lenient).expect("scalars stay lenient");
        assert_eq!(state.workspaces.len(), 1);
    }

    #[test]
    fn wrapped_id_counter_does_not_hand_out_a_duplicate_terminal_id() {
        let mut state = ProjectState::two_workspaces();
        let workspace = state.workspaces[0].id;
        state.next_terminal_id = 1;
        let existing = state
            .add_terminal(workspace, "one".to_string(), false)
            .expect("add terminal");
        state.next_terminal_id = MAX_DURABLE_SESSION_ID;

        state
            .add_terminal(workspace, "last".to_string(), false)
            .expect("add terminal");
        let wrapped = state
            .add_terminal(workspace, "wrapped".to_string(), false)
            .expect("add terminal");

        assert_ne!(wrapped, existing);
    }
}
