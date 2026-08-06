use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

#[cfg(unix)]
pub mod peer;

pub const PROTOCOL_VERSION: u16 = 10;
pub const AGENT_STATUS_SCHEMA_VERSION: u16 = 1;
pub const DEFAULT_SOCKET_NAME: &str = "mult.sock";
pub const SOCKET_PATH_ENV: &str = "MULT_SOCKET_PATH";
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SCREEN_ROWS: u16 = 1_000;
pub const MAX_SCREEN_COLS: u16 = 1_000;
pub const MAX_SCREEN_CELLS: usize = 200_000;
/// Maximum number of correlated requests a client may have awaiting results.
pub const MAX_PENDING_REQUESTS_PER_CLIENT: usize = 1_024;
/// Maximum number of completed request results retained for one resumable scope.
pub const MAX_CACHED_REQUEST_RESULTS_PER_SCOPE: usize = 4_096;

pub fn default_socket_path() -> PathBuf {
    socket_path_from(
        env::var_os(SOCKET_PATH_ENV).as_deref(),
        env::var_os("XDG_RUNTIME_DIR").as_deref(),
        socket_user_component,
    )
}

/// The socket-path policy as a pure function of its inputs.
///
/// `default_socket_path` reads two process-global environment variables; tests
/// that exercised the policy therefore had to `set_var` them, racing every
/// sibling test that resolves a socket path on another thread (G7). The policy
/// lives here instead, and the environment is read exactly once, by the wrapper.
/// `user_component` stays lazy because the fallback is the only branch that
/// needs a UID lookup.
fn socket_path_from(
    explicit: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
    user_component: impl FnOnce() -> String,
) -> PathBuf {
    if let Some(path) = explicit {
        return PathBuf::from(path);
    }

    if let Some(runtime_dir) = runtime_dir {
        return PathBuf::from(runtime_dir).join(DEFAULT_SOCKET_NAME);
    }

    // No XDG_RUNTIME_DIR: fall back to a per-user directory under /tmp. Key it
    // on the real effective UID, never on $USER/$UID — those are spoofable and
    // can collide, letting another local user predict or pre-create ("squat")
    // the path. `ensure_private_dir` verifies ownership before the socket binds.
    PathBuf::from("/tmp")
        .join(format!("mult-{}", user_component()))
        .join(DEFAULT_SOCKET_NAME)
}

/// Stable, unspoofable per-user component for the `/tmp` socket fallback.
fn socket_user_component() -> String {
    #[cfg(unix)]
    {
        peer::effective_uid().to_string()
    }

    #[cfg(not(unix))]
    {
        sanitize_socket_path_component(
            &env::var("USERNAME")
                .or_else(|_| env::var("USER"))
                .unwrap_or_else(|_| "unknown".to_string()),
        )
    }
}

#[cfg(not(unix))]
fn sanitize_socket_path_component(input: &str) -> String {
    let sanitized = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

/// Create `dir` (and any missing parents) as a private directory, then verify
/// it cannot have been pre-created by another local user.
///
/// The directory is made with mode `0700`. Because the `/tmp` fallback lives
/// under a world-writable, sticky parent, a plain `mkdir -p` that tolerates an
/// existing path would happily adopt a directory an attacker created first. So
/// after creating, every component we own is checked — via `lstat`, so symlinks
/// are never followed — to be a real directory owned by us with no group/other
/// write bit. The walk stops at a shared system root (a sticky, world-writable
/// directory such as `/tmp`) or a root-owned ancestor; a component owned by some
/// other non-root user is rejected as a squatter.
#[cfg(unix)]
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;

    let euid = peer::effective_uid();
    let mut cursor = Some(dir);
    while let Some(path) = cursor {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() {
            return Err(private_dir_error(path, "is not a directory"));
        }

        let mode = metadata.mode();
        // A sticky, world-writable directory (e.g. /tmp, /dev/shm) is a shared
        // system root; the private subtree below it is what must be ours. Stop
        // at the filesystem root as well: sandbox roots can be mapped to a
        // synthetic UID even though every mutable descendant was checked.
        if (mode & 0o1000 != 0 && mode & 0o002 != 0) || path.parent().is_none() {
            break;
        }

        let owner = metadata.uid();
        if owner == euid {
            if mode & 0o022 != 0 {
                return Err(private_dir_error(path, "is writable by group or others"));
            }
        } else if owner != 0 {
            return Err(private_dir_error(
                path,
                "is owned by another user (refusing a pre-created path)",
            ));
        }

        cursor = path.parent();
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn private_dir_error(path: &Path, problem: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "refusing to use runtime directory {}: it {problem}",
            path.display()
        ),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PaneId(pub u64);

macro_rules! nonzero_opaque_identity {
    ($name:ident, $description:literal) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 16]);

        impl $name {
            pub fn from_bytes(bytes: [u8; 16]) -> Option<Self> {
                if bytes == [0; 16] {
                    None
                } else {
                    Some(Self(bytes))
                }
            }

            pub const fn into_bytes(self) -> [u8; 16] {
                self.0
            }

            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                self.0.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let bytes = <[u8; 16]>::deserialize(deserializer)?;
                Self::from_bytes(bytes)
                    .ok_or_else(|| de::Error::custom(concat!($description, " cannot be all zero")))
            }
        }
    };
}

nonzero_opaque_identity!(StateNamespace, "state namespace");
nonzero_opaque_identity!(SessionToken, "session token");
nonzero_opaque_identity!(AgentGeneration, "agent generation");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub namespace: StateNamespace,
    pub token: SessionToken,
}

/// Opaque server-issued identity for a resumable client request scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClientScopeId([u8; 16]);

impl ClientScopeId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Opaque identity generated once for each daemon process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ServerInstanceId([u8; 16]);

impl ServerInstanceId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// A non-zero request number scoped to one [`ClientScopeId`].
///
/// Clients allocate these monotonically with [`RequestId::checked_next`] and
/// never wrap or reuse them. Retrying an idempotent request uses the exact same
/// ID and request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RequestId(NonZeroU64);

impl RequestId {
    pub const MIN: Self = Self(NonZeroU64::MIN);
    pub const MAX: Self = Self(NonZeroU64::MAX);

    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => Self::new(value),
            None => None,
        }
    }
}

/// An opaque, non-zero capability for mutating one attached pane.
///
/// The daemon allocates leases monotonically without reuse and invalidates the
/// previous lease before completing a takeover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AttachmentLease(NonZeroU64);

impl AttachmentLease {
    pub const MIN: Self = Self(NonZeroU64::MIN);
    pub const MAX: Self = Self(NonZeroU64::MAX);

    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.get().checked_add(1) {
            Some(value) => Self::new(value),
            None => None,
        }
    }
}

/// Absolute byte offset in a pane's output stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutputSequence(u64);

impl OutputSequence {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn checked_add_bytes(self, byte_count: usize) -> Option<Self> {
        let byte_count = u64::try_from(byte_count).ok()?;
        self.0.checked_add(byte_count).map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchSpec {
    Shell,
    Command(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentKind {
    Pi,
    ClaudeCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionMetadata {
    pub schema_version: u16,
    pub chat_id: u64,
    pub agent: AgentKind,
    pub generation: AgentGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Running,
    Waiting,
    /// One agent turn completed; the long-lived PTY may accept another turn.
    Finished,
    Failed,
    Exited,
}

impl AgentStatus {
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Failed | Self::Exited)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatusRecord {
    pub schema_version: u16,
    pub identity: SessionIdentity,
    pub chat_id: u64,
    pub agent: AgentKind,
    pub generation: AgentGeneration,
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatusQuery {
    pub schema_version: u16,
    pub identity: SessionIdentity,
    pub chat_id: u64,
    pub agent: AgentKind,
    pub generation: AgentGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub identity: SessionIdentity,
    pub name: String,
    pub pane: PaneId,
    pub attached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: PaneId,
    pub title: String,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForegroundProcessInfo {
    pub root_pid: Option<u32>,
    pub foreground_pid: Option<u32>,
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: u32,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello {
        protocol_version: u16,
        /// A previous scope to resume. Stateful requests may be retried only
        /// when the server confirms this scope and daemon instance.
        resume: Option<ClientScopeId>,
    },
    ListSessions {
        namespace: StateNamespace,
    },
    CreateSession {
        request_id: RequestId,
        identity: SessionIdentity,
        requested_id: Option<SessionId>,
        agent: Option<AgentSessionMetadata>,
        name: String,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
        launch: LaunchSpec,
        rows: u16,
        cols: u16,
    },
    Attach {
        request_id: RequestId,
        identity: SessionIdentity,
        session: SessionId,
        rows: u16,
        cols: u16,
    },
    /// Raw terminal input has no request ID and must never be replayed after
    /// uncertain delivery.
    Input {
        pane: PaneId,
        lease: AttachmentLease,
        bytes: Vec<u8>,
    },
    /// Prepared terminal paste bytes have no request ID and must never be
    /// replayed after uncertain delivery.
    Paste {
        pane: PaneId,
        lease: AttachmentLease,
        bytes: Vec<u8>,
    },
    Scroll {
        pane: PaneId,
        rows: i32,
    },
    ScrollToTop {
        pane: PaneId,
    },
    ScrollToBottom {
        pane: PaneId,
    },
    Resize {
        pane: PaneId,
        lease: AttachmentLease,
        rows: u16,
        cols: u16,
    },
    Detach {
        pane: PaneId,
        lease: AttachmentLease,
    },
    Stop {
        request_id: RequestId,
        identity: SessionIdentity,
        pane: PaneId,
        lease: AttachmentLease,
    },
    UpdateAgentStatus {
        request_id: RequestId,
        record: AgentStatusRecord,
    },
    GetAgentStatus {
        request_id: RequestId,
        query: AgentStatusQuery,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdentityMismatch {
    Namespace,
    SessionToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateOutcome {
    Created { session: SessionInfo },
    Error(CreateError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateError {
    /// The request ID was previously used with a different operation or body.
    RequestCollision,
    /// The request ID predates the bounded retained-result window.
    RetryExpired,
    /// A different create request already owns the requested session ID.
    SessionAlreadyExists {
        session: SessionInfo,
    },
    IdentityAlreadyExists {
        session: SessionInfo,
    },
    IdentityMismatch {
        session: SessionId,
        mismatch: IdentityMismatch,
    },
    InvalidAgentMetadata(AgentStatusError),
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachOutcome {
    Attached {
        session: SessionId,
        pane: PaneInfo,
        lease: AttachmentLease,
    },
    Error(AttachError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachError {
    RequestCollision,
    RetryExpired,
    SessionNotFound {
        session: SessionId,
    },
    IdentityMismatch {
        session: SessionId,
        mismatch: IdentityMismatch,
    },
    /// The cached attachment result's lease was invalidated by a takeover.
    Superseded,
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseOperation {
    Input,
    Paste,
    Resize,
    Detach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseRejectionReason {
    PaneMissing,
    NotOwner,
    StaleLease,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopOutcome {
    Stopped {
        exit: ExitInfo,
    },
    /// A new stop request for an absent pane is confirmed success. An exact
    /// retry instead replays the original cached outcome.
    AlreadyAbsent,
    Error(StopError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopError {
    RequestCollision,
    RetryExpired,
    IdentityMismatch {
        pane: PaneId,
        mismatch: IdentityMismatch,
    },
    LeaseRejected(LeaseRejectionReason),
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatusOutcome {
    Updated(AgentStatusRecord),
    Current(Option<AgentStatusRecord>),
    Error(AgentStatusError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatusError {
    RequestCollision,
    RetryExpired,
    WrongSchema {
        expected: u16,
        received: u16,
    },
    SessionNotFound {
        identity: SessionIdentity,
    },
    IdentityMismatch(IdentityMismatch),
    NotAgentSession {
        identity: SessionIdentity,
    },
    WrongChat {
        expected: u64,
        received: u64,
    },
    WrongAgent {
        expected: AgentKind,
        received: AgentKind,
    },
    StaleGeneration {
        current: AgentGeneration,
        received: AgentGeneration,
    },
    FinalStatusConflict {
        current: AgentStatus,
        attempted: AgentStatus,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    Hello {
        protocol_version: u16,
        server_instance: ServerInstanceId,
        client_scope: ClientScopeId,
        resumed: bool,
    },
    Sessions {
        namespace: StateNamespace,
        sessions: Vec<SessionInfo>,
    },
    CreateResult {
        request_id: RequestId,
        outcome: CreateOutcome,
    },
    /// A successful result is followed by ReplayBegin, zero or more
    /// ReplayChunk messages, then ReplayEnd for the same request and lease.
    AttachResult {
        request_id: RequestId,
        outcome: AttachOutcome,
    },
    /// Begins replay of `[first_sequence, watermark)`. A non-zero
    /// `omitted_prefix_bytes` reports suffix truncation explicitly.
    ReplayBegin {
        request_id: RequestId,
        pane: PaneId,
        lease: AttachmentLease,
        first_sequence: OutputSequence,
        watermark: OutputSequence,
        omitted_prefix_bytes: u64,
    },
    ReplayChunk {
        request_id: RequestId,
        pane: PaneId,
        lease: AttachmentLease,
        sequence: OutputSequence,
        bytes: Vec<u8>,
    },
    ReplayEnd {
        request_id: RequestId,
        pane: PaneId,
        lease: AttachmentLease,
        watermark: OutputSequence,
    },
    PtyOutput {
        pane: PaneId,
        lease: AttachmentLease,
        sequence: OutputSequence,
        bytes: Vec<u8>,
    },
    ForegroundProcess {
        pane: PaneId,
        lease: AttachmentLease,
        process: ForegroundProcessInfo,
    },
    PaneExited {
        pane: PaneId,
        lease: AttachmentLease,
        exit: ExitInfo,
    },
    StopResult {
        request_id: RequestId,
        outcome: StopOutcome,
    },
    AgentStatusResult {
        request_id: RequestId,
        outcome: AgentStatusOutcome,
    },
    TakenOver {
        pane: PaneId,
        /// The lease which was displaced, not the replacement lease.
        lease: AttachmentLease,
    },
    LeaseRejected {
        pane: PaneId,
        lease: AttachmentLease,
        operation: LeaseOperation,
        reason: LeaseRejectionReason,
    },
    /// Reserved for handshake, framing, and connection-wide protocol errors.
    Error { message: String },
}

pub fn read_message<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> io::Result<T> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(message_too_large(io::ErrorKind::InvalidData, len as u64));
    }

    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    let (message, trailing) = postcard::take_from_bytes(&payload).map_err(invalid_data)?;
    if !trailing.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message contains trailing bytes",
        ));
    }
    Ok(message)
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let payload = postcard::to_allocvec(message).map_err(invalid_data)?;
    if payload.len() > MAX_MESSAGE_BYTES {
        return Err(message_too_large(
            io::ErrorKind::InvalidInput,
            payload.len() as u64,
        ));
    }

    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too large"))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub fn bounded_screen_dimensions(rows: u16, cols: u16) -> (u16, u16) {
    let rows = rows.min(MAX_SCREEN_ROWS);
    let cols = cols.min(MAX_SCREEN_COLS);
    if rows == 0 || cols == 0 {
        return (rows, cols);
    }

    let max_rows_by_cells = (MAX_SCREEN_CELLS / usize::from(cols)).max(1);
    (rows.min(max_rows_by_cells as u16), cols)
}

fn message_too_large(kind: io::ErrorKind, len: u64) -> io::Error {
    io::Error::new(
        kind,
        format!("message too large: {len} bytes exceeds {MAX_MESSAGE_BYTES} byte limit"),
    )
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_id(value: u64) -> RequestId {
        RequestId::new(value).expect("non-zero request ID")
    }

    fn lease(value: u64) -> AttachmentLease {
        AttachmentLease::new(value).expect("non-zero attachment lease")
    }

    fn namespace(byte: u8) -> StateNamespace {
        StateNamespace::from_bytes([byte; 16]).expect("non-zero namespace")
    }

    fn token(byte: u8) -> SessionToken {
        SessionToken::from_bytes([byte; 16]).expect("non-zero token")
    }

    fn generation(byte: u8) -> AgentGeneration {
        AgentGeneration::from_bytes([byte; 16]).expect("non-zero generation")
    }

    fn identity() -> SessionIdentity {
        SessionIdentity {
            namespace: namespace(0x44),
            token: token(0x55),
        }
    }

    fn agent_metadata() -> AgentSessionMetadata {
        AgentSessionMetadata {
            schema_version: AGENT_STATUS_SCHEMA_VERSION,
            chat_id: 12,
            agent: AgentKind::Pi,
            generation: generation(0x66),
        }
    }

    fn agent_record(status: AgentStatus) -> AgentStatusRecord {
        let metadata = agent_metadata();
        AgentStatusRecord {
            schema_version: metadata.schema_version,
            identity: identity(),
            chat_id: metadata.chat_id,
            agent: metadata.agent,
            generation: metadata.generation,
            status,
        }
    }

    fn session_info() -> SessionInfo {
        SessionInfo {
            id: SessionId(7),
            identity: identity(),
            name: "fixture session".to_string(),
            pane: PaneId(17),
            attached: true,
        }
    }

    fn pane_info() -> PaneInfo {
        PaneInfo {
            id: PaneId(17),
            title: "fixture pane".to_string(),
            rows: 24,
            cols: 80,
        }
    }

    fn assert_framed_round_trip<T>(message: &T)
    where
        T: std::fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>,
    {
        let mut frame = Vec::new();
        write_message(&mut frame, message).expect("write framed fixture");

        let decoded: T = read_message(&mut frame.as_slice()).expect("read framed fixture");
        assert_eq!(&decoded, message);

        let mut reencoded = Vec::new();
        write_message(&mut reencoded, &decoded).expect("re-encode framed fixture");
        assert_eq!(reencoded, frame);
    }

    #[test]
    fn request_and_lease_values_are_non_zero_and_do_not_wrap() {
        assert_eq!(RequestId::new(0), None);
        assert_eq!(RequestId::MIN.get(), 1);
        assert_eq!(RequestId::MIN.checked_next(), RequestId::new(2));
        assert_eq!(RequestId::MAX.get(), u64::MAX);
        assert_eq!(RequestId::MAX.checked_next(), None);

        assert_eq!(AttachmentLease::new(0), None);
        assert_eq!(AttachmentLease::MIN.get(), 1);
        assert_eq!(AttachmentLease::MIN.checked_next(), AttachmentLease::new(2));
        assert_eq!(AttachmentLease::MAX.get(), u64::MAX);
        assert_eq!(AttachmentLease::MAX.checked_next(), None);
    }

    #[test]
    fn wire_decode_rejects_zero_request_and_lease_values() {
        let payload = postcard::to_allocvec(&0_u64).expect("encode zero");
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&payload);
        assert!(read_message::<RequestId>(&mut frame.as_slice()).is_err());
        assert!(read_message::<AttachmentLease>(&mut frame.as_slice()).is_err());

        for description in ["namespace", "session token", "agent generation"] {
            let payload = postcard::to_allocvec(&[0_u8; 16]).expect("encode zero identity");
            let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
            frame.extend_from_slice(&payload);
            let rejected = match description {
                "namespace" => read_message::<StateNamespace>(&mut frame.as_slice()).is_err(),
                "session token" => read_message::<SessionToken>(&mut frame.as_slice()).is_err(),
                _ => read_message::<AgentGeneration>(&mut frame.as_slice()).is_err(),
            };
            assert!(rejected, "zero {description} must be rejected");
        }
    }

    #[test]
    fn output_sequences_advance_by_exact_byte_count_without_wrapping() {
        assert_eq!(
            OutputSequence::new(40).checked_add_bytes(2),
            Some(OutputSequence::new(42))
        );
        assert_eq!(OutputSequence::new(u64::MAX).checked_add_bytes(1), None);
    }

    #[test]
    fn round_trips_every_protocol_v10_client_message_fixture() {
        let scope = ClientScopeId::from_bytes([0x11; 16]);
        let mut env = BTreeMap::new();
        env.insert("FIXTURE_KEY".to_string(), "fixture value".to_string());

        let fixtures = vec![
            ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                resume: None,
            },
            ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                resume: Some(scope),
            },
            ClientMessage::ListSessions {
                namespace: namespace(0x44),
            },
            ClientMessage::CreateSession {
                request_id: request_id(1),
                identity: identity(),
                requested_id: Some(SessionId(7)),
                agent: Some(agent_metadata()),
                name: "shell fixture".to_string(),
                cwd: Some(PathBuf::from("/fixture/cwd")),
                env: env.clone(),
                launch: LaunchSpec::Shell,
                rows: 24,
                cols: 80,
            },
            ClientMessage::CreateSession {
                request_id: RequestId::MAX,
                identity: SessionIdentity {
                    namespace: namespace(0x44),
                    token: token(0x77),
                },
                requested_id: None,
                agent: None,
                name: "command fixture".to_string(),
                cwd: None,
                env,
                launch: LaunchSpec::Command("printf '%s\\n' fixture".to_string()),
                rows: MAX_SCREEN_ROWS,
                cols: MAX_SCREEN_COLS,
            },
            ClientMessage::Attach {
                request_id: request_id(2),
                identity: identity(),
                session: SessionId(7),
                rows: 25,
                cols: 81,
            },
            ClientMessage::Input {
                pane: PaneId(17),
                lease: lease(101),
                bytes: vec![0, 0xff, b'\n'],
            },
            ClientMessage::Paste {
                pane: PaneId(17),
                lease: lease(101),
                bytes: "\u{1b}[200~λ\ntext\u{1b}[201~".as_bytes().to_vec(),
            },
            ClientMessage::Resize {
                pane: PaneId(17),
                lease: lease(101),
                rows: MAX_SCREEN_ROWS,
                cols: MAX_SCREEN_COLS,
            },
            ClientMessage::Detach {
                pane: PaneId(17),
                lease: lease(101),
            },
            ClientMessage::Stop {
                request_id: request_id(3),
                identity: identity(),
                pane: PaneId(17),
                lease: lease(101),
            },
            ClientMessage::UpdateAgentStatus {
                request_id: request_id(4),
                record: agent_record(AgentStatus::Running),
            },
            ClientMessage::UpdateAgentStatus {
                request_id: request_id(40),
                record: agent_record(AgentStatus::Idle),
            },
            ClientMessage::UpdateAgentStatus {
                request_id: request_id(41),
                record: agent_record(AgentStatus::Waiting),
            },
            ClientMessage::UpdateAgentStatus {
                request_id: request_id(42),
                record: agent_record(AgentStatus::Finished),
            },
            ClientMessage::UpdateAgentStatus {
                request_id: request_id(44),
                record: agent_record(AgentStatus::Failed),
            },
            ClientMessage::UpdateAgentStatus {
                request_id: request_id(43),
                record: agent_record(AgentStatus::Exited),
            },
            ClientMessage::GetAgentStatus {
                request_id: request_id(5),
                query: AgentStatusQuery {
                    schema_version: AGENT_STATUS_SCHEMA_VERSION,
                    identity: identity(),
                    chat_id: 12,
                    agent: AgentKind::Pi,
                    generation: generation(0x66),
                },
            },
        ];

        for fixture in &fixtures {
            assert_framed_round_trip(fixture);
        }
    }

    #[test]
    fn round_trips_every_protocol_v10_server_message_fixture() {
        let scope = ClientScopeId::from_bytes([0x22; 16]);
        let server_instance = ServerInstanceId::from_bytes([0x33; 16]);
        let session = session_info();
        let pane = pane_info();
        let mut fixtures = vec![
            ServerMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                server_instance,
                client_scope: scope,
                resumed: false,
            },
            ServerMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                server_instance,
                client_scope: scope,
                resumed: true,
            },
            ServerMessage::Sessions {
                namespace: namespace(0x44),
                sessions: vec![session.clone()],
            },
            ServerMessage::CreateResult {
                request_id: request_id(1),
                outcome: CreateOutcome::Created {
                    session: session.clone(),
                },
            },
            ServerMessage::CreateResult {
                request_id: request_id(2),
                outcome: CreateOutcome::Error(CreateError::RequestCollision),
            },
            ServerMessage::CreateResult {
                request_id: request_id(3),
                outcome: CreateOutcome::Error(CreateError::RetryExpired),
            },
            ServerMessage::CreateResult {
                request_id: request_id(4),
                outcome: CreateOutcome::Error(CreateError::SessionAlreadyExists {
                    session: session.clone(),
                }),
            },
            ServerMessage::CreateResult {
                request_id: request_id(5),
                outcome: CreateOutcome::Error(CreateError::Failed {
                    message: "spawn failed".to_string(),
                }),
            },
            ServerMessage::CreateResult {
                request_id: request_id(21),
                outcome: CreateOutcome::Error(CreateError::IdentityAlreadyExists {
                    session: session.clone(),
                }),
            },
            ServerMessage::CreateResult {
                request_id: request_id(22),
                outcome: CreateOutcome::Error(CreateError::IdentityMismatch {
                    session: SessionId(7),
                    mismatch: IdentityMismatch::Namespace,
                }),
            },
            ServerMessage::CreateResult {
                request_id: request_id(23),
                outcome: CreateOutcome::Error(CreateError::InvalidAgentMetadata(
                    AgentStatusError::WrongSchema {
                        expected: AGENT_STATUS_SCHEMA_VERSION,
                        received: 99,
                    },
                )),
            },
            ServerMessage::AttachResult {
                request_id: request_id(6),
                outcome: AttachOutcome::Attached {
                    session: SessionId(7),
                    pane: pane.clone(),
                    lease: lease(101),
                },
            },
            ServerMessage::AttachResult {
                request_id: request_id(7),
                outcome: AttachOutcome::Error(AttachError::RequestCollision),
            },
            ServerMessage::AttachResult {
                request_id: request_id(8),
                outcome: AttachOutcome::Error(AttachError::RetryExpired),
            },
            ServerMessage::AttachResult {
                request_id: request_id(9),
                outcome: AttachOutcome::Error(AttachError::SessionNotFound {
                    session: SessionId(404),
                }),
            },
            ServerMessage::AttachResult {
                request_id: request_id(10),
                outcome: AttachOutcome::Error(AttachError::Superseded),
            },
            ServerMessage::AttachResult {
                request_id: request_id(11),
                outcome: AttachOutcome::Error(AttachError::Failed {
                    message: "attach failed".to_string(),
                }),
            },
            ServerMessage::AttachResult {
                request_id: request_id(24),
                outcome: AttachOutcome::Error(AttachError::IdentityMismatch {
                    session: SessionId(7),
                    mismatch: IdentityMismatch::SessionToken,
                }),
            },
            ServerMessage::ReplayBegin {
                request_id: request_id(6),
                pane: PaneId(17),
                lease: lease(101),
                first_sequence: OutputSequence::ZERO,
                watermark: OutputSequence::new(12),
                omitted_prefix_bytes: 0,
            },
            ServerMessage::ReplayBegin {
                request_id: request_id(12),
                pane: PaneId(17),
                lease: lease(102),
                first_sequence: OutputSequence::new(4_096),
                watermark: OutputSequence::new(8_192),
                omitted_prefix_bytes: 4_096,
            },
            ServerMessage::ReplayChunk {
                request_id: request_id(6),
                pane: PaneId(17),
                lease: lease(101),
                sequence: OutputSequence::new(3),
                bytes: vec![0, 0xff, b'x'],
            },
            ServerMessage::ReplayEnd {
                request_id: request_id(6),
                pane: PaneId(17),
                lease: lease(101),
                watermark: OutputSequence::new(12),
            },
            ServerMessage::PtyOutput {
                pane: PaneId(17),
                lease: lease(101),
                sequence: OutputSequence::new(12),
                bytes: Vec::new(),
            },
            ServerMessage::PtyOutput {
                pane: PaneId(17),
                lease: lease(101),
                sequence: OutputSequence::new(12),
                bytes: vec![0, 0x1b, 0xff],
            },
            ServerMessage::ForegroundProcess {
                pane: PaneId(17),
                lease: lease(101),
                process: ForegroundProcessInfo {
                    root_pid: Some(1_000),
                    foreground_pid: Some(1_001),
                    command: Some("fixture command".to_string()),
                },
            },
            ServerMessage::PaneExited {
                pane: PaneId(17),
                lease: lease(101),
                exit: ExitInfo {
                    code: 0,
                    signal: None,
                },
            },
            ServerMessage::PaneExited {
                pane: PaneId(17),
                lease: lease(101),
                exit: ExitInfo {
                    code: 137,
                    signal: Some("SIGKILL".to_string()),
                },
            },
            ServerMessage::StopResult {
                request_id: request_id(13),
                outcome: StopOutcome::Stopped {
                    exit: ExitInfo {
                        code: 0,
                        signal: None,
                    },
                },
            },
            ServerMessage::StopResult {
                request_id: request_id(14),
                outcome: StopOutcome::AlreadyAbsent,
            },
            ServerMessage::StopResult {
                request_id: request_id(15),
                outcome: StopOutcome::Error(StopError::RequestCollision),
            },
            ServerMessage::StopResult {
                request_id: request_id(16),
                outcome: StopOutcome::Error(StopError::RetryExpired),
            },
            ServerMessage::StopResult {
                request_id: request_id(17),
                outcome: StopOutcome::Error(StopError::LeaseRejected(
                    LeaseRejectionReason::PaneMissing,
                )),
            },
            ServerMessage::StopResult {
                request_id: request_id(18),
                outcome: StopOutcome::Error(StopError::LeaseRejected(
                    LeaseRejectionReason::NotOwner,
                )),
            },
            ServerMessage::StopResult {
                request_id: request_id(19),
                outcome: StopOutcome::Error(StopError::LeaseRejected(
                    LeaseRejectionReason::StaleLease,
                )),
            },
            ServerMessage::StopResult {
                request_id: request_id(20),
                outcome: StopOutcome::Error(StopError::Failed {
                    message: "kill or wait failed".to_string(),
                }),
            },
            ServerMessage::StopResult {
                request_id: request_id(25),
                outcome: StopOutcome::Error(StopError::IdentityMismatch {
                    pane: PaneId(17),
                    mismatch: IdentityMismatch::SessionToken,
                }),
            },
            ServerMessage::AgentStatusResult {
                request_id: request_id(26),
                outcome: AgentStatusOutcome::Updated(agent_record(AgentStatus::Running)),
            },
            ServerMessage::AgentStatusResult {
                request_id: request_id(27),
                outcome: AgentStatusOutcome::Current(Some(agent_record(AgentStatus::Waiting))),
            },
            ServerMessage::AgentStatusResult {
                request_id: request_id(28),
                outcome: AgentStatusOutcome::Current(None),
            },
            ServerMessage::TakenOver {
                pane: PaneId(17),
                lease: lease(101),
            },
        ];

        for (index, error) in [
            AgentStatusError::RequestCollision,
            AgentStatusError::RetryExpired,
            AgentStatusError::WrongSchema {
                expected: AGENT_STATUS_SCHEMA_VERSION,
                received: 99,
            },
            AgentStatusError::SessionNotFound {
                identity: identity(),
            },
            AgentStatusError::IdentityMismatch(IdentityMismatch::Namespace),
            AgentStatusError::IdentityMismatch(IdentityMismatch::SessionToken),
            AgentStatusError::NotAgentSession {
                identity: identity(),
            },
            AgentStatusError::WrongChat {
                expected: 12,
                received: 13,
            },
            AgentStatusError::WrongAgent {
                expected: AgentKind::Pi,
                received: AgentKind::ClaudeCode,
            },
            AgentStatusError::StaleGeneration {
                current: generation(0x66),
                received: generation(0x77),
            },
            AgentStatusError::FinalStatusConflict {
                current: AgentStatus::Failed,
                attempted: AgentStatus::Running,
            },
            AgentStatusError::Failed {
                message: "status state failed".to_string(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            fixtures.push(ServerMessage::AgentStatusResult {
                request_id: request_id(100 + index as u64),
                outcome: AgentStatusOutcome::Error(error),
            });
        }

        for operation in [
            LeaseOperation::Input,
            LeaseOperation::Paste,
            LeaseOperation::Resize,
            LeaseOperation::Detach,
        ] {
            for reason in [
                LeaseRejectionReason::PaneMissing,
                LeaseRejectionReason::NotOwner,
                LeaseRejectionReason::StaleLease,
            ] {
                fixtures.push(ServerMessage::LeaseRejected {
                    pane: PaneId(17),
                    lease: lease(101),
                    operation,
                    reason,
                });
            }
        }

        for fixture in &fixtures {
            assert_framed_round_trip(fixture);
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn socket_path_component_sanitization_removes_path_separators() {
        assert_eq!(sanitize_socket_path_component("user-1000"), "user-1000");
        assert_eq!(sanitize_socket_path_component("../bad/user"), "___bad_user");
        assert_eq!(sanitize_socket_path_component(""), "unknown");
    }

    #[cfg(unix)]
    #[test]
    fn socket_user_component_is_the_numeric_effective_uid() {
        // The /tmp fallback must be keyed off geteuid(), not spoofable env, and
        // must contain no path separators or other surprises.
        let component = socket_user_component();
        assert!(!component.is_empty());
        assert!(component.chars().all(|ch| ch.is_ascii_digit()));
        assert_eq!(component, peer::effective_uid().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_creates_and_accepts_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_test_dir("create").join("nested");
        ensure_private_dir(&dir).expect("create private dir");
        assert!(dir.is_dir());
        let mode = fs::metadata(&dir).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Idempotent: a second call on the directory we own succeeds.
        ensure_private_dir(&dir).expect("re-accept private dir");

        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_rejects_group_or_other_writable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_test_dir("loose");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).expect("chmod");

        let error = ensure_private_dir(&dir).expect_err("world-writable dir must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_rejects_a_symlinked_directory() {
        let base = unique_test_dir("symlink");
        let target = base.join("real");
        fs::create_dir(&target).expect("create target");
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let error = ensure_private_dir(&link).expect_err("symlinked dir must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    fn unique_test_dir(label: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = env::temp_dir().join(format!(
            "mult-protocol-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("create unique test dir");
        dir
    }

    /// G7: the policy is tested through its pure seam. The old version of this
    /// test called `set_var`/`remove_var` on a process global while sibling
    /// tests resolved socket paths from other threads.
    #[test]
    fn socket_path_prefers_the_override_then_the_runtime_dir_then_a_per_uid_dir() {
        let explicit = socket_path_from(
            Some(OsStr::new("/tmp/mult-test-override.sock")),
            Some(OsStr::new("/run/user/1000")),
            || panic!("the override must not need a UID lookup"),
        );
        assert_eq!(explicit, Path::new("/tmp/mult-test-override.sock"));

        let runtime = socket_path_from(None, Some(OsStr::new("/run/user/1000")), || {
            panic!("XDG_RUNTIME_DIR must not need a UID lookup")
        });
        assert_eq!(
            runtime,
            Path::new("/run/user/1000").join(DEFAULT_SOCKET_NAME)
        );

        let fallback = socket_path_from(None, None, || "4242".to_string());
        assert_eq!(
            fallback,
            Path::new("/tmp/mult-4242").join(DEFAULT_SOCKET_NAME)
        );
    }

    #[test]
    fn read_message_rejects_oversized_payload_before_allocating() {
        let bytes = ((MAX_MESSAGE_BYTES as u32) + 1).to_be_bytes();

        let error = read_message::<ClientMessage>(&mut bytes.as_slice())
            .expect_err("oversized message should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_message_rejects_trailing_bytes_inside_frame() {
        let message = ClientMessage::Attach {
            request_id: request_id(1),
            identity: identity(),
            session: SessionId(1),
            rows: 24,
            cols: 80,
        };
        let mut payload = postcard::to_allocvec(&message).expect("serialize payload");
        payload.push(0xff);
        let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
        bytes.extend(payload);

        let error = read_message::<ClientMessage>(&mut bytes.as_slice())
            .expect_err("trailing bytes should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("trailing bytes"));
    }

    #[test]
    fn write_message_rejects_oversized_payload() {
        let message = vec![0_u8; MAX_MESSAGE_BYTES + 1];
        let mut bytes = Vec::new();

        let error = write_message(&mut bytes, &message).expect_err("reject payload");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(bytes.is_empty());
    }

    #[test]
    fn bounded_screen_dimensions_clamps_huge_sizes() {
        let (rows, cols) = bounded_screen_dimensions(u16::MAX, u16::MAX);

        assert!(rows <= MAX_SCREEN_ROWS);
        assert!(cols <= MAX_SCREEN_COLS);
        assert!(usize::from(rows) * usize::from(cols) <= MAX_SCREEN_CELLS);
    }
}
