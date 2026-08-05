use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
pub mod peer;
pub mod rand;
pub mod shell;

pub const PROTOCOL_VERSION: u16 = 11;
pub const DEFAULT_SOCKET_NAME: &str = "mult.sock";
pub const SOCKET_PATH_ENV: &str = "MULT_SOCKET_PATH";
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SCREEN_ROWS: u16 = 1_000;
pub const MAX_SCREEN_COLS: u16 = 1_000;
pub const MAX_SCREEN_CELLS: usize = 200_000;
/// The smallest screen either end may use (A13).
///
/// Not a protocol nicety: a grid one row *or* one column in size makes
/// `fnug-vt100` — the emulator the client renders every pane through — subtract
/// past zero on perfectly ordinary output (a wrapped line, an emoji, a stray
/// non-UTF-8 byte). It panics in debug and wraps in release. The floor is
/// applied here, on the shared type both ends size their screens with, so a
/// `Resize` a client puts on the wire cannot ask the daemon for a size the
/// client's own emulator would then die on. Everything from 2×2 up is clean.
pub const MIN_SCREEN_ROWS: u16 = 2;
/// See [`MIN_SCREEN_ROWS`].
pub const MIN_SCREEN_COLS: u16 = 2;
/// Initial payload reservation in [`read_message`]. Frames are overwhelmingly
/// smaller than this, so one reservation covers them; larger frames grow as
/// their bytes actually arrive rather than being committed up front.
const INITIAL_PAYLOAD_RESERVE_BYTES: usize = 16 * 1024;
/// Upper bound on how much [`read_message`] commits per read step.
const PAYLOAD_READ_CHUNK_BYTES: usize = 64 * 1024;

pub fn default_socket_path() -> PathBuf {
    socket_path_from(
        env::var_os(SOCKET_PATH_ENV).as_deref(),
        env::var_os("XDG_RUNTIME_DIR").as_deref(),
        &socket_user_component(),
    )
}

/// Pure core of [`default_socket_path`]: everything the environment contributes
/// arrives as an argument, so the precedence rules are testable without any
/// process-global `set_var` (which races every other thread in the test binary).
fn socket_path_from(
    override_path: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
    user_component: &str,
) -> PathBuf {
    if let Some(path) = override_path {
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
        .join(format!("mult-{user_component}"))
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
        match ancestor_verdict(
            path.parent().is_none(),
            metadata.file_type().is_dir(),
            metadata.mode(),
            metadata.uid(),
            euid,
        ) {
            AncestorVerdict::Accept => cursor = path.parent(),
            AncestorVerdict::Stop => break,
            AncestorVerdict::Reject(problem) => return Err(private_dir_error(path, problem)),
        }
    }

    Ok(())
}

/// What the ancestor walk in [`ensure_private_dir`] does with one component.
#[cfg(unix)]
enum AncestorVerdict {
    /// Fine; keep walking upwards.
    Accept,
    /// Fine, and there is nothing above worth checking.
    Stop,
    Reject(&'static str),
}

/// The rule the walk applies, with every input passed in — so the cases that
/// cannot be built on a real filesystem (a root owned by neither us nor root)
/// are still testable.
#[cfg(unix)]
fn ancestor_verdict(
    is_root: bool,
    is_dir: bool,
    mode: u32,
    owner: u32,
    euid: u32,
) -> AncestorVerdict {
    // The filesystem root is where the walk ends, not another component to vet.
    // Nobody "pre-creates" `/`, and an attacker who owns it owns every path the
    // check could offer instead, so testing it buys nothing. It does cost
    // something: inside a user namespace the root can map to a uid that is
    // neither ours nor 0 — a Nix build sandbox maps it to `nobody` — and the
    // ownership rule below then rejects *every* directory on the system,
    // including one we have just created 0700 ourselves (G15).
    if is_root {
        return AncestorVerdict::Stop;
    }
    if !is_dir {
        return AncestorVerdict::Reject("is not a directory");
    }
    // A sticky, world-writable directory (e.g. /tmp, /dev/shm) is a shared
    // system root; the private subtree below it is what must be ours.
    if mode & 0o1000 != 0 && mode & 0o002 != 0 {
        return AncestorVerdict::Stop;
    }
    if owner == euid {
        if mode & 0o022 != 0 {
            return AncestorVerdict::Reject("is writable by group or others");
        }
        AncestorVerdict::Accept
    } else if owner == 0 {
        AncestorVerdict::Accept
    } else {
        AncestorVerdict::Reject("is owned by another user (refusing a pre-created path)")
    }
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

/// Read a file `mult` will act on — its config, its saved state, an agent's
/// status report — with the guarantees those files need.
///
/// Every one of them is an execution or control channel of some kind: the
/// config carries shell command lines that are auto-started, the state file
/// replays terminals, the status file drives UI state. Each also sits at a path
/// an environment variable can move (`$MULT_CONFIG_PATH`, `$MULT_STATE_PATH`),
/// so the path alone proves nothing about the bytes behind it. Before reading
/// anything this therefore requires that the file:
///
/// - is not reached through a symlink (`O_NOFOLLOW`), so a planted link cannot
///   redirect the read to a file the attacker controls;
/// - is a regular file, so a FIFO or a character device cannot stall the caller
///   (the open also uses `O_NONBLOCK`, since the render thread does this) or
///   feed it an endless stream;
/// - is owned by this user — a file owned by anyone else has no business
///   telling us what to run; and
/// - is not writable by group or others, which would let a second party edit it
///   between two reads.
///
/// `max_bytes` caps the result: a `/dev/zero` symlink or a padded file cannot
/// make the caller allocate without limit, and exceeding it is an error rather
/// than a silent truncation, because a half-read config is not a config.
///
/// `NotFound` is passed through unchanged so callers can keep treating "no file
/// yet" as "use the defaults". Every other failure is a hard failure by design.
#[cfg(unix)]
pub fn read_private_file(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;

    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(private_file_error(
            path,
            io::ErrorKind::InvalidData,
            "is not a regular file",
        ));
    }
    if metadata.uid() != peer::effective_uid() {
        return Err(private_file_error(
            path,
            io::ErrorKind::PermissionDenied,
            "is owned by another user",
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(private_file_error(
            path,
            io::ErrorKind::PermissionDenied,
            "is writable by group or others",
        ));
    }

    // Read one byte past the cap so "exactly at the cap" and "over it" can be
    // told apart without trusting the size in the metadata (which a concurrent
    // writer can make stale).
    let mut contents = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > max_bytes {
        return Err(private_file_error(
            path,
            io::ErrorKind::InvalidData,
            &format!("is larger than the {max_bytes} byte limit"),
        ));
    }

    Ok(contents)
}

#[cfg(not(unix))]
pub fn read_private_file(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(private_file_error(
            path,
            io::ErrorKind::InvalidData,
            "is not a regular file",
        ));
    }

    let mut contents = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > max_bytes {
        return Err(private_file_error(
            path,
            io::ErrorKind::InvalidData,
            &format!("is larger than the {max_bytes} byte limit"),
        ));
    }

    Ok(contents)
}

fn private_file_error(path: &Path, kind: io::ErrorKind, problem: &str) -> io::Error {
    io::Error::new(
        kind,
        format!("refusing to read {}: it {problem}", path.display()),
    )
}

/// Identifies one *client instance* — one `mult` process lineage, keyed by its
/// state file — for as long as that state file lives.
///
/// Session ids are chosen by the client from its own `TerminalId`s, so they are
/// only unique within one state file: two `mult` windows both ask for session 1
/// and used to be handed the *same* pane, after which attaching evicted the
/// other window's client (A3). The daemon therefore keys every session on
/// `(instance, session)`, and a connection only ever sees sessions belonging to
/// the instance it presented in [`ClientMessage::Hello`].
///
/// The token is persisted in the client's state file so a restarted client
/// reclaims its own panes — the daemon's whole purpose. It is not a
/// capability against a same-uid attacker (who can read that file); it is what
/// stops two honest instances, or a process that merely speaks the protocol,
/// from colliding with or taking over sessions that are not theirs (C12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InstanceId(pub u64);

impl InstanceId {
    /// `0` is reserved for "no token presented" and is refused by the daemon,
    /// so a client that fails to allocate one cannot land in a namespace it
    /// would share with every other such client.
    pub const UNSET: Self = Self(0);

    pub fn is_set(self) -> bool {
        self.0 != Self::UNSET.0
    }
}

/// Identifies one session — and therefore one pane — within an
/// [`InstanceId`]'s namespace.
///
/// A session owns exactly one PTY, so the daemon's session id and its pane id
/// were always the same number wearing two newtypes, converted back and forth
/// at a dozen call sites and defended by a linear scan for a mismatch that
/// could not happen (A11). One id now names both; multi-pane sessions would
/// reintroduce the distinction, and nothing is building them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u64);

/// The high bit of a session id, set on the ids of runtime-only panes.
///
/// A client owns two kinds of PTY: durable workspace terminals, which it
/// persists, and the agent process behind a chat, which it does not. Both are
/// named to the daemon by one [`SessionId`], so the client partitions the id
/// space with this flag. The daemon never reads it — it is the client's own
/// convention — but the encoding lives here so both directions are written once,
/// next to each other, and the *inverse can fail* instead of silently
/// reinterpreting an out-of-range id as the other kind (F4).
pub const RUNTIME_SESSION_FLAG: u64 = 1 << 63;

/// Largest id either half of the space can hold. `0` is excluded: it is the
/// "not allocated yet" value, never a live session.
pub const MAX_DURABLE_SESSION_ID: u64 = RUNTIME_SESSION_FLAG - 1;

/// Which half of the session id space an id belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SessionKind {
    /// A workspace terminal, persisted in the client's state file.
    Durable,
    /// A chat's agent process — runtime-only, never persisted.
    Runtime,
}

impl SessionId {
    /// The session id naming `id` in `kind`'s half of the space.
    ///
    /// `None` for an `id` outside `1..=MAX_DURABLE_SESSION_ID`, which is the
    /// whole point: an id with the flag bit already set would otherwise encode
    /// as an id of the *other* kind, and that collision is what this replaced.
    pub const fn for_kind(kind: SessionKind, id: u64) -> Option<Self> {
        if id == 0 || id > MAX_DURABLE_SESSION_ID {
            return None;
        }
        match kind {
            SessionKind::Durable => Some(Self(id)),
            SessionKind::Runtime => Some(Self(id | RUNTIME_SESSION_FLAG)),
        }
    }

    /// The inverse of [`SessionId::for_kind`].
    ///
    /// Fallible on purpose: a pane id a daemon invented — `0`, or the bare flag
    /// — has no valid reading, and inventing one for it is exactly the type
    /// confusion this exists to prevent.
    pub const fn split(self) -> Option<(SessionKind, u64)> {
        let id = self.0 & MAX_DURABLE_SESSION_ID;
        if id == 0 {
            return None;
        }
        if self.0 & RUNTIME_SESSION_FLAG == 0 {
            Some((SessionKind::Durable, id))
        } else {
            Some((SessionKind::Runtime, id))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchSpec {
    Shell,
    Command(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub attached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: SessionId,
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
        /// The client instance whose session namespace this connection uses.
        /// See [`InstanceId`]; `InstanceId::UNSET` is refused.
        instance: InstanceId,
    },
    /// Keepalive. Carries nothing: it exists so the daemon can tell an idle but
    /// live client from one that is gone and whose socket never closed, and so
    /// drop the latter's thread instead of pinning it forever (A10).
    Ping,
    ListSessions,
    CreateSession {
        requested_id: Option<SessionId>,
        name: String,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
        launch: LaunchSpec,
        rows: u16,
        cols: u16,
    },
    Attach {
        session: SessionId,
        rows: u16,
        cols: u16,
    },
    Input {
        pane: SessionId,
        bytes: Vec<u8>,
    },
    Resize {
        pane: SessionId,
        rows: u16,
        cols: u16,
    },
    Detach,
    Stop {
        pane: SessionId,
    },
}

/// Why the daemon refused, or could not carry out, a request.
///
/// This is the wire's machine-readable failure vocabulary. Every
/// [`ServerMessage::Error`] carries one, and it — never the accompanying
/// `message` — is what a client branches on. Adding a variant is a protocol
/// change: a client that does not know a code must treat it as
/// [`RejectCode::Unspecified`] would be treated, which is why decoding an
/// unknown discriminant is a hard error rather than a silent mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RejectCode {
    /// A message other than `Hello` arrived before the protocol hello.
    HelloRequired,
    /// The client's `PROTOCOL_VERSION` does not match the daemon's.
    ProtocolMismatch,
    /// The hello carried no instance token (`InstanceId::UNSET`).
    InstanceTokenRequired,
    /// A second hello tried to move an established connection into a different
    /// session namespace.
    InstanceMismatch,
    /// The daemon is already serving its maximum number of connections.
    ConnectionLimit,
    /// The daemon is already hosting its maximum number of live sessions
    /// across every instance.
    SessionLimit,
    /// The request named a session this connection's namespace does not hold —
    /// it exited, the daemon restarted, or it belongs to another instance.
    UnknownSession,
    /// The pane was attached by another connection of the same instance, which
    /// has now taken it over. Sent to the connection that lost it.
    SessionBusy,
    /// A pane's input queue is full because its child has stopped reading
    /// stdin, so the input was refused rather than dropped or blocked on.
    InputRefused,
    /// Creating a session failed: a duplicate id, the session cap, or a PTY
    /// that could not be opened or spawned.
    SessionCreateFailed,
    /// An operation on an existing pane failed (attach, resize, write, stop).
    PaneOperationFailed,
    /// A failure with no more specific code. Present so a daemon always has
    /// something honest to send rather than mislabelling.
    Unspecified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    Hello {
        protocol_version: u16,
    },
    Sessions(Vec<SessionInfo>),
    Attached {
        session: SessionId,
        panes: Vec<PaneInfo>,
    },
    PtyScrollback {
        pane: SessionId,
        bytes: Vec<u8>,
    },
    PtyOutput {
        pane: SessionId,
        bytes: Vec<u8>,
    },
    ForegroundProcess {
        pane: SessionId,
        process: ForegroundProcessInfo,
    },
    PaneExited {
        pane: SessionId,
        exit: ExitInfo,
    },
    /// A failure report. `pane` names the pane the failure belongs to when the
    /// server knows it, so the client can attribute it instead of guessing;
    /// `None` means the failure is connection-wide (a bad hello, a message that
    /// never named a pane) and belongs to a global surface, not to a pane.
    ///
    /// `code` is the machine-readable half and the only part a client may
    /// branch on; `message` is for the user and may be reworded freely. Before
    /// protocol 11 there was no code, and the client picked an `io::ErrorKind`
    /// by substring-matching `message` — a protocol contract written in English
    /// prose, which Slice 9 had already broken by replacing the "already
    /// attached" rejection with a takeover (F8).
    Error {
        pane: Option<SessionId>,
        code: RejectCode,
        message: String,
    },
}

pub fn read_message<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> io::Result<T> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(message_too_large(io::ErrorKind::InvalidData, len as u64));
    }
    if len == 0 {
        // No encodable message is zero bytes long (postcard writes at least the
        // variant tag), so an empty frame is a protocol error, not an empty read.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty message frame",
        ));
    }

    // Grow the buffer as the payload actually arrives instead of committing the
    // declared frame size before reading a single byte: a peer that writes only
    // a length header would otherwise park this reader on an allocation of up
    // to `MAX_MESSAGE_BYTES`, once per connection.
    let mut payload = Vec::new();
    payload.reserve_exact(len.min(INITIAL_PAYLOAD_RESERVE_BYTES));
    let mut filled = 0;
    while filled < len {
        let target = len.min(filled + PAYLOAD_READ_CHUNK_BYTES);
        payload.resize(target, 0);
        reader.read_exact(&mut payload[filled..target])?;
        filled = target;
    }
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

/// Clamp a requested screen size into the range both ends can hold: at least
/// [`MIN_SCREEN_ROWS`]×[`MIN_SCREEN_COLS`], at most `MAX_SCREEN_ROWS`×
/// `MAX_SCREEN_COLS`, and no more than `MAX_SCREEN_CELLS` cells in total.
///
/// The lower bound used to be zero here and `.max(1)` at each caller, which is
/// exactly one short of what the emulator tolerates — see [`MIN_SCREEN_ROWS`].
pub fn bounded_screen_dimensions(rows: u16, cols: u16) -> (u16, u16) {
    let rows = rows.clamp(MIN_SCREEN_ROWS, MAX_SCREEN_ROWS);
    let cols = cols.clamp(MIN_SCREEN_COLS, MAX_SCREEN_COLS);

    // The cell budget can only take rows away, never below the floor: the
    // widest allowed screen is 1000 cols, so it never asks for fewer than 200.
    let max_rows_by_cells =
        (MAX_SCREEN_CELLS / usize::from(cols)).max(usize::from(MIN_SCREEN_ROWS));
    (rows.min(max_rows_by_cells as u16), cols)
}

fn message_too_large(kind: io::ErrorKind, len: u64) -> io::Error {
    io::Error::new(
        kind,
        format!("message too large: {len} bytes exceeds {MAX_MESSAGE_BYTES} byte limit"),
    )
}

/// An `InvalidData` error carrying a decoder's own message.
///
/// Shared with the client's config and state decoders, which report the same
/// class of failure and used to spell this out for themselves (F20).
pub fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_round_trip_through_both_halves_of_the_space() {
        for kind in [SessionKind::Durable, SessionKind::Runtime] {
            for id in [1, 7, MAX_DURABLE_SESSION_ID] {
                let session = SessionId::for_kind(kind, id).expect("encode a valid id");
                assert_eq!(session.split(), Some((kind, id)));
            }
        }
        // The two halves never collide, which the old high-bit encoding could
        // not promise for an id that already had the flag bit set.
        assert_ne!(
            SessionId::for_kind(SessionKind::Durable, 7),
            SessionId::for_kind(SessionKind::Runtime, 7)
        );
    }

    #[test]
    fn out_of_range_ids_cannot_be_encoded_and_zero_ids_cannot_be_decoded() {
        for kind in [SessionKind::Durable, SessionKind::Runtime] {
            assert_eq!(SessionId::for_kind(kind, 0), None);
            assert_eq!(SessionId::for_kind(kind, RUNTIME_SESSION_FLAG), None);
            assert_eq!(SessionId::for_kind(kind, u64::MAX), None);
        }
        // A daemon that names a pane `0` (in either half) is reporting garbage,
        // not a durable terminal.
        assert_eq!(SessionId(0).split(), None);
        assert_eq!(SessionId(RUNTIME_SESSION_FLAG).split(), None);
    }

    #[test]
    fn instance_id_zero_is_the_unset_token() {
        assert!(!InstanceId::UNSET.is_set());
        assert!(InstanceId(1).is_set());
    }

    #[test]
    fn round_trips_a_hello_with_its_instance_token() {
        let message = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            instance: InstanceId(0x1234_5678_9abc_def0),
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).expect("write message");
        let decoded: ClientMessage = read_message(&mut bytes.as_slice()).expect("read message");

        assert_eq!(decoded, message);
    }

    #[test]
    fn round_trips_messages_with_length_prefix() {
        let message = ClientMessage::Attach {
            session: SessionId(1),
            rows: 24,
            cols: 80,
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).expect("write message");
        let decoded: ClientMessage = read_message(&mut bytes.as_slice()).expect("read message");

        assert_eq!(decoded, message);
    }

    #[test]
    fn round_trips_raw_pty_output() {
        let message = ServerMessage::PtyOutput {
            pane: SessionId(7),
            bytes: b"hello\x1b[31m".to_vec(),
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).expect("write message");
        let decoded: ServerMessage = read_message(&mut bytes.as_slice()).expect("read message");

        assert_eq!(decoded, message);
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
        assert_eq!(component, unsafe { libc::geteuid() }.to_string());
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

    /// G15: the walk stops at the filesystem root instead of vetting it. Under
    /// a user namespace `/` can be owned by neither us nor root — a Nix build
    /// sandbox maps it to `nobody` — and vetting it rejected every directory on
    /// the system, including one we had just created 0700 ourselves. That state
    /// cannot be built on a real filesystem, which is why the rule takes its
    /// inputs as arguments.
    #[cfg(unix)]
    #[test]
    fn the_ancestor_walk_stops_at_the_root_without_judging_it() {
        // A root owned by `nobody`: the case that made `nix flake check` red.
        assert!(matches!(
            ancestor_verdict(true, true, 0o755, 65_534, 1_000),
            AncestorVerdict::Stop
        ));
        // Everything below the root is still vetted exactly as before.
        assert!(matches!(
            ancestor_verdict(false, true, 0o700, 1_000, 1_000),
            AncestorVerdict::Accept
        ));
        assert!(matches!(
            ancestor_verdict(false, true, 0o755, 0, 1_000),
            AncestorVerdict::Accept
        ));
        assert!(matches!(
            ancestor_verdict(false, true, 0o777, 1_000, 1_000),
            AncestorVerdict::Reject("is writable by group or others")
        ));
        assert!(matches!(
            ancestor_verdict(false, true, 0o700, 65_534, 1_000),
            AncestorVerdict::Reject(_)
        ));
        assert!(matches!(
            ancestor_verdict(false, false, 0o700, 1_000, 1_000),
            AncestorVerdict::Reject("is not a directory")
        ));
        // A sticky world-writable parent (/tmp) ends the walk.
        assert!(matches!(
            ancestor_verdict(false, true, 0o1777, 0, 1_000),
            AncestorVerdict::Stop
        ));
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
    #[test]
    fn read_private_file_accepts_a_private_regular_file() {
        let dir = unique_test_dir("read-ok");
        let path = dir.join("config.json");
        fs::write(&path, b"{}").expect("write file");

        assert_eq!(read_private_file(&path, 1024).expect("read"), b"{}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_private_file_refuses_to_follow_a_symlink() {
        let dir = unique_test_dir("read-symlink");
        let target = dir.join("real.json");
        fs::write(&target, b"{}").expect("write target");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        // O_NOFOLLOW: the link is never traversed, so the planted target is
        // never read. The exact errno differs by platform; what matters is that
        // it is an error and not the target's contents.
        let error = read_private_file(&link, 1024).expect_err("symlink must be refused");
        assert_ne!(error.kind(), io::ErrorKind::NotFound);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_private_file_rejects_group_or_other_writable_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_test_dir("read-loose");
        let path = dir.join("config.json");
        fs::write(&path, b"{}").expect("write file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("chmod");

        let error = read_private_file(&path, 1024).expect_err("world-writable must be refused");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("writable by group or others"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_private_file_rejects_a_fifo() {
        let dir = unique_test_dir("read-fifo");
        let path = dir.join("config.json");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("c path");
        // SAFETY: `c_path` is a valid NUL-terminated path this test owns.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        // Without O_NONBLOCK this open would block forever on a missing writer;
        // with it, the file-type check refuses the FIFO instead of reading it.
        let error = read_private_file(&path, 1024).expect_err("a FIFO must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not a regular file"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_private_file_rejects_contents_over_the_cap() {
        let dir = unique_test_dir("read-cap");
        let path = dir.join("state.json");
        fs::write(&path, vec![b'a'; 128]).expect("write file");

        // At the cap is fine; one byte over is a hard error, never a truncation.
        assert_eq!(
            read_private_file(&path, 128).expect("read at cap").len(),
            128
        );
        let error = read_private_file(&path, 127).expect_err("over the cap must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("byte limit"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_private_file_reports_a_missing_file_as_not_found() {
        let dir = unique_test_dir("read-missing");

        let error =
            read_private_file(&dir.join("absent.json"), 1024).expect_err("missing file errors");

        // Callers distinguish "no file yet" (use defaults) from every other
        // failure (refuse to start), so this kind must survive the helper.
        assert_eq!(error.kind(), io::ErrorKind::NotFound);

        let _ = fs::remove_dir_all(&dir);
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

    #[test]
    fn socket_path_prefers_the_override_then_the_runtime_dir_then_the_tmp_fallback() {
        let override_path = OsStr::new("/tmp/mult-test-override.sock");
        let runtime_dir = OsStr::new("/run/user/1000");

        // The override wins even when a runtime dir is available.
        assert_eq!(
            socket_path_from(Some(override_path), Some(runtime_dir), "1000"),
            PathBuf::from("/tmp/mult-test-override.sock")
        );
        assert_eq!(
            socket_path_from(None, Some(runtime_dir), "1000"),
            PathBuf::from("/run/user/1000").join(DEFAULT_SOCKET_NAME)
        );
        assert_eq!(
            socket_path_from(None, None, "1000"),
            PathBuf::from("/tmp/mult-1000").join(DEFAULT_SOCKET_NAME)
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

    /// A13: the lower bound is two, not one. A peer can put `rows: 1` on the
    /// wire, and one row or one column is a size the client's emulator
    /// overflows on.
    #[test]
    fn bounded_screen_dimensions_raises_tiny_sizes_to_the_emulator_floor() {
        for (rows, cols) in [(0, 0), (1, 1), (1, 80), (80, 1), (0, 200), (200, 0)] {
            let (rows, cols) = bounded_screen_dimensions(rows, cols);

            assert!(rows >= MIN_SCREEN_ROWS, "{rows} rows is below the floor");
            assert!(cols >= MIN_SCREEN_COLS, "{cols} cols is below the floor");
        }

        // The floor never overrides a size that is already legal.
        assert_eq!(bounded_screen_dimensions(2, 2), (2, 2));
        assert_eq!(bounded_screen_dimensions(24, 80), (24, 80));

        // Nor does the cell budget push a screen back under it.
        let (rows, cols) = bounded_screen_dimensions(MAX_SCREEN_ROWS, MAX_SCREEN_COLS);
        assert!(rows >= MIN_SCREEN_ROWS && cols >= MIN_SCREEN_COLS);
    }
}
