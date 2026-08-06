use std::{
    collections::{BTreeMap, HashMap},
    env, fs, io,
    net::Shutdown,
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        mpsc::{self, Receiver, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use mult_protocol::{
    default_socket_path,
    peer::verify_peer_is_self,
    read_message,
    shell::{default_shell, shell_command_args},
    write_message, ClientMessage, ForegroundProcessInfo, InstanceId, LaunchSpec, RejectCode,
    ServerMessage, SessionId, PROTOCOL_VERSION, SOCKET_PATH_ENV,
};
use vt100::{MouseProtocolEncoding, MouseProtocolMode, Parser};

use crate::model::PtyKey;

/// What went wrong talking to the daemon.
///
/// Every variant is a distinct thing a caller may want to react to, named once
/// here instead of being recovered downstream. Before this, everything was an
/// `io::Error` and the interesting distinctions were carried in prose: the
/// attach path picked its `ErrorKind` with `message.contains("already
/// attached")` against text the *daemon* formatted, so the two ends were
/// coupled through English that neither compiler checked. Slice 9 then replaced
/// that rejection with a takeover and the substring stopped ever matching,
/// silently, with nothing failing (F8).
///
/// `io::Result` survives only at the true I/O boundary — connecting a socket,
/// spawning the daemon, reading a file — where an `io::Error` is the honest
/// answer rather than a lossy container.
#[derive(Debug)]
pub enum PtyError {
    /// There is no daemon connection yet. A connection attempt has been started
    /// in the background; this particular message did not go out.
    NotConnected,
    /// The connection broke while sending. It has been dropped and a fresh one
    /// is being established.
    Disconnected(io::Error),
    /// The daemon stopped reading for long enough that a frame was cut in half,
    /// so the stream is no longer parseable and the connection was torn down.
    WriteStalled(Duration),
    /// A bounded wait on the daemon expired. `what` names the wait.
    Timeout { what: &'static str, after: Duration },
    /// The daemon refused, or could not carry out, the request. `code` is the
    /// machine-readable reason and the only part that may be branched on.
    Rejected { code: RejectCode, message: String },
    /// The daemon speaks a different protocol version.
    ProtocolMismatch { server: u16 },
    /// A `PtyKey` that carries no id any wire session could have (F4).
    UnroutableKey(PtyKey),
    /// The PTY already has a daemon attachment, so starting it again would
    /// abandon the running one.
    AlreadyRunning(PtyKey),
    /// The socket writer mutex was left poisoned by a panicking thread.
    WriterPoisoned,
    /// A genuine I/O failure at the socket boundary.
    Io(io::Error),
}

impl std::fmt::Display for PtyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => write!(f, "not connected to mult-server"),
            Self::Disconnected(error) => write!(f, "mult-server connection lost: {error}"),
            Self::WriteStalled(after) => {
                write!(f, "mult-server stopped reading input after {after:?}")
            }
            Self::Timeout { what, after } => {
                write!(f, "timed out after {after:?} waiting for {what}")
            }
            Self::Rejected { code, message } => write!(f, "mult-server rejected ({code:?}): {message}"),
            Self::ProtocolMismatch { server } => write!(
                f,
                "mult-server protocol version {server} is incompatible with client version {PROTOCOL_VERSION}; restart mult-server"
            ),
            Self::UnroutableKey(key) => write!(f, "{key:?} has no session id"),
            Self::AlreadyRunning(key) => write!(f, "{key:?} already has a server attachment"),
            Self::WriterPoisoned => write!(f, "server socket writer lock poisoned"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for PtyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Disconnected(error) | Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl PtyError {
    /// Whether this failure means the connection is gone, as opposed to the
    /// request having been refused on a connection that is still up.
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::NotConnected | Self::Disconnected(_))
    }

    /// The daemon's own reason when there is one, so a failure that started as
    /// a rejection keeps its code as it is turned into a [`PtyEvent::Error`].
    /// Everything the client decided for itself is `Unspecified`.
    pub fn reject_code(&self) -> RejectCode {
        match self {
            Self::Rejected { code, .. } => *code,
            Self::ProtocolMismatch { .. } => RejectCode::ProtocolMismatch,
            _ => RejectCode::Unspecified,
        }
    }
}

/// The boundary conversion. `runtime::run` and `main` report through
/// `io::Result`, so a `PtyError` that reaches them becomes an `io::Error` whose
/// kind is derived from the variant rather than from its text.
impl From<PtyError> for io::Error {
    fn from(error: PtyError) -> Self {
        let kind = match &error {
            PtyError::NotConnected => io::ErrorKind::NotConnected,
            PtyError::Disconnected(_) => io::ErrorKind::ConnectionAborted,
            PtyError::WriteStalled(_) | PtyError::Timeout { .. } => io::ErrorKind::TimedOut,
            PtyError::Rejected { .. } => io::ErrorKind::InvalidInput,
            PtyError::ProtocolMismatch { .. } => io::ErrorKind::InvalidData,
            PtyError::UnroutableKey(_) => io::ErrorKind::InvalidInput,
            PtyError::AlreadyRunning(_) => io::ErrorKind::AlreadyExists,
            PtyError::WriterPoisoned => io::ErrorKind::Other,
            PtyError::Io(inner) => inner.kind(),
        };
        io::Error::new(kind, error.to_string())
    }
}

pub type PtyResult<T> = Result<T, PtyError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtySpawn {
    pub pty: PtyKey,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub size: PtyDimensions,
}

/// A PTY's screen size, clamped by construction to something the emulator can
/// actually hold (A13).
///
/// The fields are private and the only constructor is [`PtyDimensions::new`],
/// because the floor is not advisory: a one-row *or* one-column `vt100` grid
/// panics with "attempt to subtract with overflow" (`grid.rs:637` when a row
/// wraps with nowhere to scroll, `screen.rs:788` when a double-width character
/// is measured against a single column) on input as ordinary as a stray
/// non-UTF-8 byte, an emoji or a line long enough to wrap. Debug builds panic
/// outright — with the terminal in raw mode, so the user is left with an unusable
/// shell — and release builds have overflow checks off and wrap instead, which
/// is not better. It is an upstream defect in `fnug-vt100`; the clamp here is
/// our workaround, and it has to survive a dependency bump.
///
/// Everything from 2×2 up was probed clean, so 2 is the floor rather than some
/// larger round number: it is the smallest size that is *correct*, and a bigger
/// one would silently mis-size real panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyDimensions {
    rows: u16,
    cols: u16,
}

/// The smallest grid `fnug-vt100` handles without arithmetic overflow.
pub const MIN_PTY_ROWS: u16 = mult_protocol::MIN_SCREEN_ROWS;
/// The smallest grid `fnug-vt100` handles without arithmetic overflow.
pub const MIN_PTY_COLS: u16 = mult_protocol::MIN_SCREEN_COLS;

impl PtyDimensions {
    /// Dimensions for a pane of `rows` × `cols`, raised to the emulator's floor
    /// and lowered to the wire's ceiling. The daemon applies the same bounds to
    /// a `Resize` it receives, so both ends of a session agree on the size a
    /// pane was actually given.
    pub fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = mult_protocol::bounded_screen_dimensions(rows, cols);
        Self { rows, cols }
    }

    pub fn rows(self) -> u16 {
        self.rows
    }

    pub fn cols(self) -> u16 {
        self.cols
    }
}

/// Something that happened to a PTY, as observed by the event loop.
///
/// `Scrollback`/`Output` carry only the size of the chunk, never the bytes: the
/// payload is already applied to the terminal's parser by the time the event is
/// emitted, and no consumer reads it back out of the queue. Cloning megabytes
/// of PTY output into an event nobody inspects was pure per-chunk allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    Scrollback {
        pty: PtyKey,
        byte_count: usize,
    },
    Output {
        pty: PtyKey,
        byte_count: usize,
    },
    Exited {
        pty: PtyKey,
        status: PtyExit,
    },
    /// A failure reported by the daemon. `pty` is `None` when the failure is
    /// connection-wide and belongs to no pane; it must not be attributed to an
    /// arbitrary PTY. `code` is the daemon's machine-readable reason, carried
    /// through so a consumer can branch on it instead of on `message` (F8).
    Error {
        pty: Option<PtyKey>,
        code: RejectCode,
        message: String,
    },
    /// Something worth telling the user that is not a failure — currently only
    /// "the daemon connection came back", which is news precisely because its
    /// loss was reported as an error (B6).
    Notice {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyExit {
    pub code: u32,
    pub signal: Option<String>,
}

pub struct PtyRuntime {
    socket_path: PathBuf,
    /// This client's session namespace on the daemon (A3). Persisted in the
    /// state file, so a restarted `mult` reclaims its own panes and a *second*
    /// `mult` cannot be handed them.
    instance: InstanceId,
    connection: Option<ServerConnection>,
    /// Where the (background) establishment of `connection` currently stands.
    connect: ConnectState,
    /// Spawns asked for while no connection existed, replayed once one lands.
    /// See [`PtyRuntime::start`].
    pending_spawns: Vec<PtySpawn>,
    /// When the last message went out, so the keepalive only writes when the
    /// connection has genuinely been quiet (A10).
    last_write: Instant,
    /// Whether the user has been told the connection is down. Gates the
    /// "reconnected" notice so a healthy start-up says nothing at all.
    reported_disconnect: bool,
    /// Everything the client knows about one PTY, in one entry (F2).
    ///
    /// This used to be eight maps keyed by the same `PtyKey`, so every
    /// lifecycle operation had to remember which subset of them to touch — the
    /// old `remove_terminal` cleared seven, `reset_parser` four, `stop` three —
    /// and any omission was a state leak nothing would notice.
    panes: HashMap<PtyKey, PtyPane>,
    /// Reverse lookup for [`PtyPane::session`], so a `ServerMessage` naming a
    /// pane finds its PTY without scanning. Kept in step by `attach`/`detach`
    /// and `remove_pty`, which are the only writers.
    pane_index: HashMap<SessionId, PtyKey>,
    pending_events: Vec<PtyEvent>,
    // The PTY currently being created by `start`, if any. It is excluded
    // from reconnect re-attach because its session does not exist on the server
    // until `start`'s own CreateSession completes.
    starting: Option<PtyKey>,
    // Set when `drain_events` stopped on its per-frame budget with messages
    // still queued, so the caller knows to redraw and come back for the rest.
    deferred_work: bool,
    // Most recent connection-wide daemon error (one with no pane). Kept for a
    // global status surface; without one, attributing it to some pane would be
    // a lie.
    last_server_error: Option<String>,
}

const SERVER_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const ATTACH_ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a socket write from the render thread may stall before the
/// connection is declared broken.
///
/// The daemon no longer blocks its socket reader on a PTY write (A2), so this
/// should never fire; it is the belt to that fix's braces, because a blocking
/// `write_all` on a socket whose peer has stopped reading has no bound of its
/// own and the render thread is what would hang. A frame interrupted this way
/// leaves the stream desynchronised, so the connection is dropped rather than
/// reused — see [`PtyRuntime::send`].
const SERVER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the client tells an otherwise-silent daemon that it is still here,
/// well inside the daemon's `CLIENT_IDLE_TIMEOUT` (A10).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
/// Minimum gap between connection attempts after one fails, so an unreachable
/// daemon costs one connect per second rather than one per frame (B6).
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
/// Ceiling for an autospawned daemon's socket to appear; see `wait_for_server`.
const SERVER_SPAWN_TIMEOUT: Duration = Duration::from_secs(15);
/// Depth of the reader thread → UI thread queue. Each entry can hold a full
/// PTY read (~8 KiB), so this is also the client's worst-case queued memory
/// while the UI thread is busy; keep it small and let the socket's own flow
/// control (and the daemon's queue) absorb the rest.
const SERVER_EVENT_QUEUE_CAPACITY: usize = 256;
/// Per-frame work budget for `drain_events`. A pane that produces output faster
/// than vt100 consumes it refills the queue as fast as it drains, so an
/// unbudgeted drain never returns and the UI stops answering keystrokes.
/// Whatever is left stays queued for the next tick.
const DRAIN_MAX_MESSAGES_PER_FRAME: usize = 128;
const DRAIN_MAX_BYTES_PER_FRAME: usize = 256 * 1024;
const TERMINAL_SCROLLBACK_LINES: usize = 5_000;
const TERMINAL_MAX_CSI_SEQUENCE_BYTES: usize = 128;
/// Ceiling on terminal query auto-responses generated per input chunk. A pane
/// printing `\x1b[6n` in a loop would otherwise turn one 8 KiB read into
/// thousands of replies, each generated on the render thread.
const TERMINAL_MAX_RESPONSES_PER_CHUNK: usize = 8;
const PRIMARY_DEVICE_ATTRIBUTES_RESPONSE: &[u8] = b"\x1b[?1;2c";
const DEVICE_STATUS_OK_RESPONSE: &[u8] = b"\x1b[0n";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
/// xterm button bytes reported for the scroll wheel (bit 6 set marks a wheel
/// event; the low bit distinguishes up from down).
const WHEEL_UP_BUTTON: u8 = 64;
const WHEEL_DOWN_BUTTON: u8 = 65;

/// Where establishing a daemon connection currently stands.
///
/// Connecting used to happen inline on the render thread: `connect_or_spawn`
/// (up to 15 s waiting for an autospawned daemon's socket), the protocol hello
/// (2 s) and the attach acknowledgement (2 s) were all paid by whichever
/// keystroke happened to be the one that noticed the connection was gone (B6).
/// They happen on a worker thread now; the render thread only ever polls this.
enum ConnectState {
    /// Connected, or nothing has asked for a connection yet.
    Idle,
    /// A worker is connecting. The channel yields exactly one result.
    InFlight(Receiver<io::Result<EstablishedConnection>>),
    /// The last attempt failed; do not start another before this instant.
    Backoff { retry_at: Instant },
    /// This runtime must never open a connection. Only [`PtyRuntime::new_offline`]
    /// sets it, so a test that drives the client cannot reach — or create
    /// sessions on — the developer's own running daemon.
    Disabled,
}

/// A connection a worker thread finished building: the socket, its shutdown
/// handle, and the receiving end of the reader thread it already started.
struct EstablishedConnection {
    writer: UnixStream,
    socket: UnixStream,
    receiver: Receiver<ServerMessage>,
}

struct ServerConnection {
    writer: Arc<Mutex<UnixStream>>,
    receiver: Receiver<ServerMessage>,
    /// A clone of the same socket, kept solely so the connection can be shut
    /// down. Dropping our handles is not enough: the reader thread owns its own
    /// dup of the fd and stays parked in `read_message` on a still-open socket
    /// until the server independently evicts it, so repeated reconnects would
    /// accumulate one live thread and fd pair each.
    socket: UnixStream,
}

impl ServerConnection {
    /// Shut the socket down in both directions. This is what wakes the reader
    /// thread: its next `read_message` sees EOF and the thread exits.
    fn shutdown(&self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

impl Drop for ServerConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
struct TerminalResponseDetector {
    state: TerminalResponseState,
    /// Parameter bytes of the CSI sequence currently being scanned. Inline and
    /// held across states because the sequence is already length-bounded, and a
    /// heap allocation per CSI is thousands of allocations per frame while a
    /// full-screen TUI child redraws.
    csi: [u8; TERMINAL_MAX_CSI_SEQUENCE_BYTES],
    csi_len: usize,
}

impl Default for TerminalResponseDetector {
    fn default() -> Self {
        Self {
            state: TerminalResponseState::default(),
            csi: [0; TERMINAL_MAX_CSI_SEQUENCE_BYTES],
            csi_len: 0,
        }
    }
}

/// A query from the PTY child that this terminal emulator answers on the
/// child's behalf. The reply bytes are produced later, from the screen state at
/// the query point, so a suppressed query costs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalQuery {
    /// `CSI c` / `ESC Z` — primary device attributes.
    DeviceAttributes,
    /// `CSI 5 n` — device status report.
    DeviceStatus,
    /// `CSI 6 n` (or its private `CSI ? 6 n` form) — cursor position report.
    CursorPosition { private: bool },
}

/// Per-chunk allowance for terminal query auto-responses.
///
/// One chunk is one PTY read, so a program stuck in a query loop gets at most a
/// handful of replies per read instead of one per query — and at most one
/// cursor report, since every reply in a chunk would report a cursor the child
/// has not observed moving anyway.
#[derive(Debug)]
struct TerminalResponseBudget {
    remaining: usize,
    cursor_position_reported: bool,
}

impl Default for TerminalResponseBudget {
    fn default() -> Self {
        Self {
            remaining: TERMINAL_MAX_RESPONSES_PER_CHUNK,
            cursor_position_reported: false,
        }
    }
}

impl TerminalResponseBudget {
    /// Append the reply for `query` to `out` if the chunk's allowance permits.
    fn push_response(&mut self, query: TerminalQuery, screen: &vt100::Screen, out: &mut Vec<u8>) {
        if self.remaining == 0 {
            return;
        }
        match query {
            TerminalQuery::DeviceAttributes => {
                out.extend_from_slice(PRIMARY_DEVICE_ATTRIBUTES_RESPONSE)
            }
            TerminalQuery::DeviceStatus => out.extend_from_slice(DEVICE_STATUS_OK_RESPONSE),
            TerminalQuery::CursorPosition { private } => {
                if self.cursor_position_reported {
                    return;
                }
                self.cursor_position_reported = true;
                out.extend_from_slice(&cursor_position_report(screen, private));
            }
        }
        self.remaining -= 1;
    }
}

#[derive(Debug, Default)]
struct TerminalCommandTracker {
    input: String,
    last: Option<String>,
    state: TerminalInputTrackState,
}

#[derive(Debug, Default)]
enum TerminalInputTrackState {
    #[default]
    Ground,
    Escape,
    Csi,
}

#[derive(Debug, Default)]
enum TerminalResponseState {
    #[default]
    Ground,
    Escape,
    /// Inside a CSI sequence; the parameter bytes live in the detector's inline
    /// `csi` buffer rather than in the state itself.
    Csi,
    CsiIgnored,
    String {
        esc_seen: bool,
    },
    IgnoreOne,
}

/// Whether a connection attempt may start a `mult-server` that is not running.
///
/// Autospawning forks a daemon that deliberately outlives the client, so it is
/// never something a code path should be able to do by accident. This used to be
/// a bare `bool` threaded through three layers of call, where `false` at a call
/// site said nothing about what it was switching off — and the runtime's
/// `Default` impl reached the `true` path, so a stray `..Default::default()`
/// would have forked a process (F3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnPolicy {
    /// Start `mult-server` when the socket is absent. Only ever reached from an
    /// action the user asked for.
    Autospawn,
    /// Connect to a daemon that is already listening, or fail. What the render
    /// loop's own background reconnect uses.
    ConnectOnly,
}

impl PtyRuntime {
    /// The runtime the application runs on: it starts connecting in the
    /// background straight away and autospawns a daemon if there is none.
    ///
    /// Deliberately not `Result`: startup no longer waits for the daemon, so
    /// "the socket is not there yet" is a state the UI renders (and reports
    /// through the status line) rather than an error the caller must handle.
    /// Also deliberately not `Default`: every construction names its socket and
    /// its [`SpawnPolicy`], because two of the three constructors here must
    /// never reach a daemon at all.
    pub fn autospawning(socket_path: Option<PathBuf>, instance: InstanceId) -> Self {
        Self::with_socket_path(
            socket_path.unwrap_or_else(default_socket_path),
            instance,
            SpawnPolicy::Autospawn,
        )
    }

    /// A runtime that will never connect to anything.
    ///
    /// Used by tests that drive the client without a daemon. Connecting is
    /// disabled rather than merely absent, because the default socket path may
    /// well have a *real* daemon behind it on a developer's machine, and a test
    /// must not create sessions there.
    pub fn new_offline() -> Self {
        Self::offline_with_pending_events(Vec::new())
    }

    /// A disconnected runtime that already holds `events`.
    ///
    /// The pane-less connect failure that `with_socket_path` queues is only
    /// produced by a real failed connect, which a test cannot ask for without
    /// either a socket or an autospawn attempt. This is the same state,
    /// constructed directly.
    pub fn offline_with_pending_events(events: Vec<PtyEvent>) -> Self {
        let mut runtime = Self::disconnected(default_socket_path(), InstanceId::UNSET, events);
        runtime.connect = ConnectState::Disabled;
        runtime
    }

    pub fn with_socket_path(
        socket_path: PathBuf,
        instance: InstanceId,
        policy: SpawnPolicy,
    ) -> Self {
        let mut runtime = Self::disconnected(socket_path, instance, Vec::new());
        runtime.begin_connect(policy);
        runtime
    }

    /// Connect synchronously, failing if the daemon cannot be reached.
    ///
    /// The blocking form, kept for callers that have nowhere to render progress
    /// (the integration suite) and want a hard answer. The application uses
    /// [`PtyRuntime::with_socket_path`], which never blocks the render thread.
    pub fn connect_to_socket(socket_path: PathBuf, instance: InstanceId) -> io::Result<Self> {
        let mut runtime = Self::disconnected(socket_path, instance, Vec::new());
        let established =
            establish_connection(&runtime.socket_path, instance, SpawnPolicy::Autospawn)?;
        runtime.install_connection(established);
        Ok(runtime)
    }

    fn disconnected(
        socket_path: PathBuf,
        instance: InstanceId,
        pending_events: Vec<PtyEvent>,
    ) -> Self {
        Self {
            socket_path,
            instance,
            connect: ConnectState::Idle,
            pending_spawns: Vec::new(),
            last_write: Instant::now(),
            reported_disconnect: false,
            connection: None,
            panes: HashMap::new(),
            pane_index: HashMap::new(),
            pending_events,
            starting: None,
            deferred_work: false,
            last_server_error: None,
        }
    }
}

impl PtySpawn {
    pub fn shell(pty: PtyKey, cwd: Option<PathBuf>, env: BTreeMap<String, String>) -> Self {
        Self {
            pty,
            program: default_shell(),
            args: Vec::new(),
            cwd,
            env,
            size: PtyDimensions::default(),
        }
    }

    pub fn command_line(
        pty: PtyKey,
        command: String,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            pty,
            program: default_shell(),
            args: shell_command_args(command),
            cwd,
            env,
            size: PtyDimensions::default(),
        }
    }
}

impl Default for PtyDimensions {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

/// Everything the client tracks for one PTY.
///
/// An entry exists as soon as anything is known about a PTY — output arrived,
/// a size was set, a foreground process was reported — and lives until
/// [`PtyRuntime::remove_pty`] drops it. Attachment (`session`), history
/// (`parser`) and outcome (`exit`) all have different lifetimes, so each is an
/// `Option` in its own right rather than the whole entry coming and going.
#[derive(Default)]
struct PtyPane {
    /// The daemon session this PTY is attached to, or `None` while it is
    /// unattached — before the first `Attach`, after `stop`, after a
    /// `PaneExited`, or after the connection dropped.
    session: Option<SessionId>,
    /// The terminal emulator holding this PTY's screen and scrollback. Kept
    /// across a lost connection so the last output stays on screen, and rebuilt
    /// from the daemon's replay on re-attach.
    parser: Option<Parser>,
    /// Escape-sequence scanner for the queries this emulator answers on the
    /// child's behalf. Reset with the parser: a half-scanned sequence means
    /// nothing once the screen is rebuilt.
    responder: TerminalResponseDetector,
    /// Whether any output has ever reached the parser, so a pane that printed
    /// only whitespace is still "not blank".
    saw_output: bool,
    /// How the PTY finished, once it has.
    exit: Option<PtyExit>,
    /// The daemon's last report of what is running in the pane, used to decide
    /// whether input is going to the shell or to a child.
    foreground: Option<ForegroundProcessInfo>,
    /// Reconstructed shell command line, shown as the terminal's label.
    commands: TerminalCommandTracker,
}

impl PtyPane {
    /// Start this PTY's screen afresh at `size`.
    ///
    /// The parser, the escape scanner, the "has produced output" flag and the
    /// recorded exit all describe *one run* of the PTY, so they are reset
    /// together; the attachment, the foreground process and the command history
    /// survive, exactly as the four separate calls this replaced did.
    fn reset(&mut self, size: PtyDimensions) {
        self.parser = Some(new_parser(size));
        self.responder = TerminalResponseDetector::default();
        self.saw_output = false;
        self.exit = None;
    }

    /// The parser, creating a default-sized one if this PTY has none yet.
    fn parser_mut(&mut self) -> &mut Parser {
        self.parser
            .get_or_insert_with(|| new_parser(PtyDimensions::default()))
    }
}

/// The one place a screen is built from a size (A13).
///
/// `PtyDimensions` cannot hold a size below the emulator's floor, so routing
/// every `Parser::new` through it means no call site has to remember the clamp
/// — which is how the one-row case survived in the first place, `.max(1)` being
/// applied consistently and consistently one too low.
fn new_parser(size: PtyDimensions) -> Parser {
    Parser::new(size.rows(), size.cols(), TERMINAL_SCROLLBACK_LINES)
}

impl PtyRuntime {
    /// Whether this terminal has a live server attachment — or a spawn waiting
    /// on a connection that is still being established, which the UI must also
    /// show as running or it would start the same terminal again every frame.
    pub fn is_running(&self, pty: PtyKey) -> bool {
        self.session_of(pty).is_some() || self.has_pending_spawn(pty)
    }

    /// The daemon session `terminal` is attached to, if it is attached.
    fn session_of(&self, pty: PtyKey) -> Option<SessionId> {
        self.panes.get(&pty).and_then(|pane| pane.session)
    }

    /// This PTY's entry, creating an empty one if it has none yet.
    fn pane_mut(&mut self, pty: PtyKey) -> &mut PtyPane {
        self.panes.entry(pty).or_default()
    }

    /// Record that `terminal` is now attached to `session`, replacing any
    /// previous attachment (and its index entry) so the two never disagree.
    fn attach(&mut self, pty: PtyKey, session: SessionId) {
        self.detach(pty);
        self.pane_mut(pty).session = Some(session);
        self.pane_index.insert(session, pty);
    }

    /// Drop `terminal`'s attachment, returning the session it had. Everything
    /// else about the PTY — its screen, its exit, its command history — is left
    /// alone; only the link to the daemon goes.
    fn detach(&mut self, pty: PtyKey) -> Option<SessionId> {
        let session = self.panes.get_mut(&pty)?.session.take()?;
        self.pane_index.remove(&session);
        Some(session)
    }

    fn has_pending_spawn(&self, pty: PtyKey) -> bool {
        self.pending_spawns.iter().any(|spawn| spawn.pty == pty)
    }

    pub fn parser(&self, pty: PtyKey) -> Option<&Parser> {
        self.panes.get(&pty)?.parser.as_ref()
    }

    pub fn pty_exit_status(&self, pty: PtyKey) -> Option<&PtyExit> {
        self.panes.get(&pty)?.exit.as_ref()
    }

    pub fn pty_last_command(&self, pty: PtyKey) -> Option<&str> {
        self.panes.get(&pty)?.commands.last_command()
    }

    #[cfg(test)]
    pub fn mark_running_for_test(&mut self, pty: PtyKey) {
        let session = session_for_key(pty).expect("test key has a session id");
        self.attach(pty, session);
    }

    #[cfg(test)]
    pub fn record_exit_status_for_test(&mut self, pty: PtyKey, status: PtyExit) {
        self.pane_mut(pty).exit = Some(status);
    }

    pub fn reset_parser(&mut self, pty: PtyKey, size: PtyDimensions) {
        self.pane_mut(pty).reset(size);
    }

    /// Forget a PTY entirely: its attachment, its screen, its exit, everything.
    ///
    /// One `remove` now, because there is one entry. It used to be seven
    /// removals that had to be kept in step by hand.
    pub fn remove_pty(&mut self, pty: PtyKey) {
        self.detach(pty);
        self.panes.remove(&pty);
        self.pending_spawns.retain(|spawn| spawn.pty != pty);
    }

    pub fn process_pty_output(&mut self, pty: PtyKey, bytes: &[u8]) {
        self.feed_pty_output(pty, bytes, false);
    }

    fn feed_pty_output(&mut self, pty: PtyKey, bytes: &[u8], respond: bool) {
        if bytes.is_empty() {
            return;
        }

        let responses = {
            let pane = self.panes.entry(pty).or_default();
            let PtyPane {
                parser, responder, ..
            } = pane;
            let parser = parser.get_or_insert_with(|| new_parser(PtyDimensions::default()));
            let responses = if respond {
                let mut budget = TerminalResponseBudget::default();
                feed_parser_with_responder(parser, responder, &mut budget, bytes)
            } else {
                // No terminal queries to answer on this path (scrollback replay,
                // local echo, system lines), so feed the whole slice in one call
                // rather than one parser dispatch per byte — a replay can be
                // megabytes.
                parser.process(bytes);
                Vec::new()
            };
            clamp_parser_scrollback(parser);
            pane.saw_output = true;
            responses
        };

        if responses.is_empty() {
            return;
        }
        // Every reply this chunk earned goes out as one `Input` message. Each
        // reply used to be its own socket write from the render thread.
        if let Some(session) = self.session_of(pty) {
            let _ = self.send_input_inner(pty, session, &responses, false);
        }
    }

    /// Write a `[mult]` status line into a pane's emulator.
    ///
    /// Some of these lines quote strings the *server* chose — a
    /// `ServerMessage::Error` text, an `ExitInfo::signal` name — so the message
    /// is sanitized before it reaches the parser. Otherwise a rogue daemon
    /// could have `mult` paint arbitrary escape sequences into a pane of its
    /// choosing: clear it, reposition the cursor, recolour it, or forge a
    /// convincing `[mult]` line of its own. Rendering goes through vt100 cells,
    /// so this never escapes to the host terminal, but a spoofed pane is
    /// deception enough.
    pub fn append_pty_system_line(&mut self, pty: PtyKey, message: impl AsRef<str>) {
        let line = format!("[mult] {}\r\n", sanitize_system_line(message.as_ref()));
        self.process_pty_output(pty, line.as_bytes());
    }

    pub fn pty_lines(&self, pty: PtyKey) -> Vec<String> {
        let Some(parser) = self.parser(pty) else {
            return Vec::new();
        };
        terminal_screen_rows(parser)
    }

    pub fn pty_output_is_blank(&self, pty: PtyKey) -> bool {
        let Some(pane) = self.panes.get(&pty) else {
            return true;
        };
        if pane.saw_output {
            return false;
        }
        pane.parser
            .as_ref()
            .map(|parser| {
                terminal_screen_rows(parser)
                    .iter()
                    .all(|line| line.is_empty())
            })
            .unwrap_or(true)
    }

    /// Start a terminal on the daemon.
    ///
    /// With a connection in hand this creates the session and waits for the
    /// attach acknowledgement, so a rejected start is reported to the caller.
    /// With no connection it *queues* the spawn and returns `Ok`: connecting is
    /// a background activity now (B6), and failing the start because the daemon
    /// is two hundred milliseconds from being ready would leave the terminal
    /// stopped for good. A queued spawn runs the moment the connection lands; if
    /// the connection never lands, [`PtyRuntime::connection_lost`] retires it
    /// with an exit event exactly like a terminal that had been running.
    pub fn start(&mut self, spawn: PtySpawn) -> PtyResult<()> {
        if self.is_running(spawn.pty) {
            return Err(PtyError::AlreadyRunning(spawn.pty));
        }

        if self.connection.is_none() {
            self.begin_connect(SpawnPolicy::Autospawn);
            self.queue_spawn(spawn);
            return Ok(());
        }

        // Mark this terminal as starting so a reconnect triggered mid-start does
        // not re-attach it before its CreateSession exists on the server. Cleared
        // on every exit path.
        self.starting = Some(spawn.pty);
        let result = self.start_attached(spawn);
        self.starting = None;
        result
    }

    /// Hold a spawn until there is a connection to run it on.
    fn queue_spawn(&mut self, spawn: PtySpawn) {
        self.restart_pane(spawn.pty, spawn.size);
        let pty = spawn.pty;
        self.pending_spawns.retain(|pending| pending.pty != pty);
        self.pending_spawns.push(spawn);
        self.append_pty_system_line(pty, "waiting for mult-server...");
    }

    /// Run every queued spawn now that a connection exists. A spawn the daemon
    /// refuses is retired with an exit event, so the app stops showing it as
    /// running instead of waiting on a terminal that will never produce output.
    fn flush_pending_spawns(&mut self, events: &mut Vec<PtyEvent>) {
        for spawn in std::mem::take(&mut self.pending_spawns) {
            let pty = spawn.pty;
            self.starting = Some(pty);
            let result = self.start_attached(spawn);
            self.starting = None;
            if let Err(error) = result {
                let status = PtyExit {
                    code: 1,
                    signal: Some("failed to start on mult-server".to_string()),
                };
                self.pane_mut(pty).exit = Some(status.clone());
                events.push(PtyEvent::Error {
                    pty: Some(pty),
                    code: error.reject_code(),
                    message: format!("failed to start pty: {error}"),
                });
                events.push(PtyEvent::Exited { pty, status });
            }
        }
    }

    fn start_attached(&mut self, spawn: PtySpawn) -> PtyResult<()> {
        self.ensure_connected()?;
        self.restart_pane(spawn.pty, spawn.size);
        let session = session_for_key(spawn.pty)?;
        let launch = launch_spec(&spawn);
        let name = session_name(session, &launch);
        self.attach(spawn.pty, session);

        let result = self
            .send(ClientMessage::CreateSession {
                requested_id: Some(session),
                name,
                cwd: spawn.cwd.clone(),
                env: spawn.env.clone(),
                launch,
                rows: spawn.size.rows(),
                cols: spawn.size.cols(),
            })
            .and_then(|()| {
                self.send(ClientMessage::Attach {
                    session,
                    rows: spawn.size.rows(),
                    cols: spawn.size.cols(),
                })
            })
            .and_then(|()| self.wait_for_attach_ack(session, ATTACH_ACK_TIMEOUT));

        if result.is_err() {
            self.detach(spawn.pty);
        }
        result
    }

    pub fn stop(&mut self, pty: PtyKey) -> PtyResult<bool> {
        let Some(session) = self.session_of(pty) else {
            // A spawn that never reached the daemon is stopped by forgetting it.
            let had_pending = self.has_pending_spawn(pty);
            self.pending_spawns.retain(|spawn| spawn.pty != pty);
            return Ok(had_pending);
        };

        self.send(ClientMessage::Stop { pane: session })?;
        self.detach(pty);
        self.pane_mut(pty).exit = None;
        Ok(true)
    }

    pub fn send_input(&mut self, pty: PtyKey, input: &[u8]) -> PtyResult<bool> {
        let Some(session) = self.session_of(pty) else {
            return Ok(false);
        };
        self.send_input_inner(pty, session, input, true)?;
        Ok(true)
    }

    fn send_input_inner(
        &mut self,
        pty: PtyKey,
        pane: SessionId,
        input: &[u8],
        track_command: bool,
    ) -> PtyResult<()> {
        if !input.is_empty() {
            let alternate_screen = self
                .parser(pty)
                .is_some_and(|parser| parser.screen().alternate_screen());
            if let Some(parser) = self.panes.get_mut(&pty).and_then(|p| p.parser.as_mut()) {
                parser.set_scrollback(0);
            }
            if track_command && !alternate_screen && self.pty_accepts_shell_input(pty) {
                self.pane_mut(pty).commands.record_input(input);
            }
        }
        self.send(ClientMessage::Input {
            pane,
            bytes: input.to_vec(),
        })?;
        Ok(())
    }

    fn pty_accepts_shell_input(&self, pty: PtyKey) -> bool {
        let Some(process) = self
            .panes
            .get(&pty)
            .and_then(|pane| pane.foreground.as_ref())
        else {
            return true;
        };

        match (process.root_pid, process.foreground_pid) {
            (Some(root_pid), Some(foreground_pid)) => root_pid == foreground_pid,
            _ => true,
        }
    }

    pub fn send_paste(&mut self, pty: PtyKey, text: &str) -> PtyResult<bool> {
        let use_bracketed = self
            .parser(pty)
            .is_some_and(|parser| parser.screen().bracketed_paste());
        let bytes = terminal_paste_bytes(text, use_bracketed);
        self.send_input(pty, &bytes)
    }

    /// Whether the program in `terminal` has switched on xterm mouse
    /// reporting. When it has, the wheel belongs to the program (it scrolls its
    /// own view) rather than to our local scrollback — which for an
    /// alternate-screen app like Claude Code holds nothing to scroll anyway.
    pub fn pty_reports_mouse(&self, pty: PtyKey) -> bool {
        self.parser(pty)
            .is_some_and(|parser| parser.screen().mouse_protocol_mode() != MouseProtocolMode::None)
    }

    /// Forward one scroll-wheel notch to a mouse-reporting program, encoded in
    /// the protocol it requested. `col`/`row` are 1-based, screen-relative cell
    /// coordinates. Returns false when the terminal has no live parser/pane or
    /// is not reporting the mouse.
    pub fn forward_wheel(&mut self, pty: PtyKey, up: bool, col: u16, row: u16) -> bool {
        let Some(parser) = self.parser(pty) else {
            return false;
        };
        let screen = parser.screen();
        if screen.mouse_protocol_mode() == MouseProtocolMode::None {
            return false;
        }
        let encoding = screen.mouse_protocol_encoding();
        let button = if up {
            WHEEL_UP_BUTTON
        } else {
            WHEEL_DOWN_BUTTON
        };
        let bytes = encode_mouse_event(encoding, button, col.max(1), row.max(1));
        let Some(session) = self.session_of(pty) else {
            return false;
        };
        self.send_input_inner(pty, session, &bytes, false).is_ok()
    }

    /// Scroll the local scrollback. Infallible — nothing leaves this process —
    /// so it reports only whether the view moved, rather than wrapping that in
    /// an `io::Result` that could never be `Err`.
    pub fn scroll_up(&mut self, pty: PtyKey, rows: usize) -> bool {
        // Clamp before narrowing: an unsaturated `as i32` on a row count with
        // bit 31 set produces a negative delta, i.e. a scroll in the opposite
        // direction. `scroll_down` clamps the same way.
        self.scroll_parser(pty, rows.min(i32::MAX as usize) as i32)
    }

    pub fn scroll_down(&mut self, pty: PtyKey, rows: usize) -> bool {
        self.scroll_parser(pty, -(rows.min(i32::MAX as usize) as i32))
    }

    pub fn resize(&mut self, pty: PtyKey, size: PtyDimensions) -> PtyResult<()> {
        self.resize_parser(pty, size);
        let Some(session) = self.session_of(pty) else {
            return Ok(());
        };
        self.send(ClientMessage::Resize {
            pane: session,
            rows: size.rows(),
            cols: size.cols(),
        })
    }

    /// Whether the last `drain_events` stopped on its per-frame budget with
    /// work still queued. The caller should redraw and drain again rather than
    /// idle, or the leftover output would sit unrendered until the next event.
    pub fn has_deferred_work(&self) -> bool {
        self.deferred_work
    }

    /// The most recent daemon error that named no pane, if one has arrived
    /// since the last call. Connection-wide failures have no terminal to be
    /// written into, so they are held here for a global status surface.
    pub fn take_last_server_error(&mut self) -> Option<String> {
        self.last_server_error.take()
    }

    pub fn drain_events(&mut self) -> Vec<PtyEvent> {
        let mut events = std::mem::take(&mut self.pending_events);
        self.poll_connect(&mut events);
        self.send_keepalive_if_due();
        let was_connected = self.connection.is_some();
        let mut disconnected = !was_connected;
        let mut messages = 0_usize;
        let mut bytes = 0_usize;
        self.deferred_work = false;
        // Adjacent output for the same pane is merged into one parser feed:
        // consecutive 8 KiB reads from a busy pane arrive as separate messages
        // but are one contiguous byte stream, and one `process` call over the
        // whole run is markedly cheaper than one per message.
        let mut pending_output: Option<(SessionId, Vec<u8>)> = None;

        while self.connection.is_some() {
            if messages >= DRAIN_MAX_MESSAGES_PER_FRAME || bytes >= DRAIN_MAX_BYTES_PER_FRAME {
                // Over budget: leave the rest queued for the next tick and tell
                // the caller there is more to come.
                self.deferred_work = true;
                break;
            }
            let message = self
                .connection
                .as_ref()
                .map(|connection| connection.receiver.try_recv());
            match message {
                Some(Ok(ServerMessage::PtyOutput {
                    pane,
                    bytes: mut chunk,
                })) => {
                    messages += 1;
                    bytes += chunk.len();
                    match pending_output.as_mut() {
                        Some((pending_pane, buffered)) if *pending_pane == pane => {
                            buffered.append(&mut chunk)
                        }
                        _ => {
                            if let Some((pending_pane, buffered)) = pending_output.take() {
                                self.handle_pty_output(pending_pane, buffered, &mut events);
                            }
                            pending_output = Some((pane, chunk));
                        }
                    }
                }
                Some(Ok(message)) => {
                    messages += 1;
                    // Ordering matters: buffered output must reach the parser
                    // before whatever follows it (an exit, a scrollback replay).
                    if let Some((pane, buffered)) = pending_output.take() {
                        self.handle_pty_output(pane, buffered, &mut events);
                    }
                    self.handle_server_message(message, &mut events);
                }
                Some(Err(TryRecvError::Empty)) => break,
                Some(Err(TryRecvError::Disconnected)) | None => {
                    disconnected = true;
                    break;
                }
            }
        }

        if let Some((pane, buffered)) = pending_output.take() {
            self.handle_pty_output(pane, buffered, &mut events);
        }

        if disconnected {
            self.close_connection();
            // Only react to the moment a *live* connection drops, not to every
            // idle frame while already disconnected (which would busy-loop). The
            // daemon's sessions outlive the connection, so a reconnect is started
            // in the background and re-attaches on success; if the daemon is
            // truly gone, the failing attempt retires the terminals rather than
            // leaving them frozen as "running".
            if was_connected {
                // Autospawn stays a deliberate, send-driven action, never
                // something the render loop does behind the user's back.
                self.begin_connect(SpawnPolicy::ConnectOnly);
            }
        }

        events
    }

    /// Collect a finished background connect, if there is one.
    ///
    /// This is the only place a connection is adopted or a failure is reported,
    /// so the render thread's whole involvement in connecting is one `try_recv`
    /// per frame.
    fn poll_connect(&mut self, events: &mut Vec<PtyEvent>) {
        let ConnectState::InFlight(receiver) = &self.connect else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            // The worker died without answering, which no retry of ours fixes
            // any faster than the normal backoff.
            Err(TryRecvError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the mult-server connection attempt did not finish",
            )),
        };

        match result {
            Ok(established) => {
                self.connect = ConnectState::Idle;
                self.install_connection(established);
                self.reattach_ptys();
                self.flush_pending_spawns(events);
                if self.reported_disconnect {
                    self.reported_disconnect = false;
                    events.push(PtyEvent::Notice {
                        message: "reconnected to mult-server".to_string(),
                    });
                }
            }
            Err(error) => {
                self.connect = ConnectState::Backoff {
                    retry_at: Instant::now() + RECONNECT_BACKOFF,
                };
                self.reported_disconnect = true;
                events.push(PtyEvent::Error {
                    // Nothing is attached, so this belongs to no terminal.
                    pty: None,
                    code: RejectCode::Unspecified,
                    message: format!("failed to connect to mult-server: {error}"),
                });
                self.connection_lost();
                events.append(&mut self.pending_events);
            }
        }
    }

    /// Start establishing a connection on a worker thread unless one is already
    /// in flight, already established, or the last failure is still cooling off.
    fn begin_connect(&mut self, policy: SpawnPolicy) {
        if self.connection.is_some() {
            return;
        }
        match &self.connect {
            ConnectState::Disabled | ConnectState::InFlight(_) => return,
            ConnectState::Backoff { retry_at } if Instant::now() < *retry_at => return,
            _ => {}
        }

        let (sender, receiver) = mpsc::sync_channel(1);
        let socket_path = self.socket_path.clone();
        let instance = self.instance;
        thread::spawn(move || {
            // The receiver is gone when the runtime was dropped mid-connect;
            // the established socket then drops with this message.
            let _ = sender.send(establish_connection(&socket_path, instance, policy));
        });
        self.connect = ConnectState::InFlight(receiver);
    }

    fn install_connection(&mut self, established: EstablishedConnection) {
        // Shut the previous connection down before adopting the new one: its
        // reader thread is still blocked in `read_message` on a socket the
        // server has no reason to close, so replacing the handle alone would
        // leak a thread and a pair of fds per reconnect.
        self.close_connection();
        self.last_write = Instant::now();
        self.connection = Some(ServerConnection {
            writer: Arc::new(Mutex::new(established.writer)),
            receiver: established.receiver,
            socket: established.socket,
        });
    }

    /// Tell the daemon we are still here when nothing else has (A10).
    ///
    /// One small write every [`KEEPALIVE_INTERVAL`] on an otherwise silent
    /// connection, which is what lets the daemon put a deadline on connections
    /// that are genuinely gone without also dropping a client that is merely
    /// idle. A failed keepalive needs no handling: the reader notices the same
    /// break on this frame or the next.
    fn send_keepalive_if_due(&mut self) {
        if self.connection.is_none() || self.last_write.elapsed() < KEEPALIVE_INTERVAL {
            return;
        }
        let _ = self.write(&ClientMessage::Ping);
    }

    fn wait_for_attach_ack(&mut self, session: SessionId, timeout: Duration) -> PtyResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(PtyError::Timeout {
                    what: "attach confirmation",
                    after: timeout,
                });
            }
            let remaining = deadline.saturating_duration_since(now);
            let message = {
                let Some(connection) = self.connection.as_ref() else {
                    return Err(PtyError::NotConnected);
                };
                connection.receiver.recv_timeout(remaining)
            };

            match message {
                Ok(ServerMessage::Attached {
                    session: attached, ..
                }) if attached == session => return Ok(()),
                // Only an error about *this* session (or one that names no pane,
                // i.e. a connection-wide failure) says anything about this
                // attach; another pane's failure is routed like any other
                // message so it does not abort an attach it has nothing to do
                // with.
                Ok(ServerMessage::Error {
                    pane,
                    code,
                    message,
                }) if pane.is_none_or(|pane| pane.0 == session.0) => {
                    return Err(PtyError::Rejected { code, message });
                }
                Ok(message) => {
                    let mut events = Vec::new();
                    self.handle_server_message(message, &mut events);
                    self.pending_events.extend(events);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(PtyError::Timeout {
                        what: "attach confirmation",
                        after: timeout,
                    });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.close_connection();
                    return Err(PtyError::Disconnected(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "mult-server disconnected before attach confirmation",
                    )));
                }
            }
        }
    }

    /// Whether this PTY has an entry at all — a screen, an attachment, a
    /// history. A resize for a pane that has none is a resize of nothing; see
    /// [`PtyRuntime::resize_parser`].
    pub fn has_pane(&self, pty: PtyKey) -> bool {
        self.panes.contains_key(&pty)
    }

    /// Resize an existing pane's screen. A pane with no entry is left alone.
    ///
    /// Deliberately not `pane_mut`: the resize the loop sends names whatever the
    /// last layout selected, which is one tick stale after a delete, so
    /// `or_default` allocated a fresh 5000-line vt100 parser for a `TerminalId`
    /// that no longer exists and could never be selected again — one leaked
    /// screen buffer per deleted pane, for the life of the session (F12).
    fn resize_parser(&mut self, pty: PtyKey, size: PtyDimensions) {
        let Some(pane) = self.panes.get_mut(&pty) else {
            return;
        };
        let parser = pane.parser_mut();
        parser.set_size(size.rows(), size.cols());
        clamp_parser_scrollback(parser);
    }

    /// Prepare a PTY to run again: a fresh screen, and no memory of what the
    /// previous run was doing.
    ///
    /// The command tracker and the last foreground report describe the process
    /// that is going away, so they go with it — which is what `queue_spawn` and
    /// `start_attached` each spelled out as three separate calls.
    fn restart_pane(&mut self, pty: PtyKey, size: PtyDimensions) {
        let pane = self.pane_mut(pty);
        pane.reset(size);
        pane.foreground = None;
        pane.commands = TerminalCommandTracker::default();
    }

    fn scroll_parser(&mut self, pty: PtyKey, rows: i32) -> bool {
        if rows == 0 {
            return false;
        }
        let Some(parser) = self
            .panes
            .get_mut(&pty)
            .and_then(|pane| pane.parser.as_mut())
        else {
            return false;
        };
        let old = parser.screen().scrollback();
        let next = if rows > 0 {
            old.saturating_add(rows as usize)
        } else {
            old.saturating_sub(rows.unsigned_abs() as usize)
        };
        parser.set_scrollback(next.min(TERMINAL_SCROLLBACK_LINES));
        clamp_parser_scrollback(parser);
        parser.screen().scrollback() != old
    }

    fn handle_server_message(&mut self, message: ServerMessage, events: &mut Vec<PtyEvent>) {
        match message {
            ServerMessage::Hello { .. }
            | ServerMessage::Sessions(_)
            | ServerMessage::Attached { .. } => {}
            ServerMessage::ForegroundProcess { pane, process } => {
                if let Some(pty) = self.key_for_pane(pane) {
                    self.record_foreground_process(pty, process);
                }
            }
            ServerMessage::PtyScrollback { pane, bytes } => {
                if let Some(pty) = self.key_for_pane(pane) {
                    self.feed_pty_output(pty, &bytes, false);
                    events.push(PtyEvent::Scrollback {
                        pty,
                        byte_count: bytes.len(),
                    });
                }
            }
            ServerMessage::PtyOutput { pane, bytes } => {
                self.handle_pty_output(pane, bytes, events);
            }
            ServerMessage::PaneExited { pane, exit } => {
                if let Some(pty) = self.key_for_pane(pane) {
                    self.detach(pty);
                    let status = PtyExit {
                        code: exit.code,
                        signal: exit.signal,
                    };
                    self.pane_mut(pty).exit = Some(status.clone());
                    events.push(PtyEvent::Exited { pty, status });
                }
            }
            ServerMessage::Error {
                pane,
                code,
                message,
            } => {
                // Attribute the failure only when the daemon named a pane we
                // actually have: picking an arbitrary entry from the map (or a
                // terminal id that cannot exist) wrote failures into whichever
                // terminal the hash order happened to yield.
                let pty = pane.and_then(|pane| self.key_for_pane(pane));
                if pty.is_none() {
                    self.last_server_error = Some(message.clone());
                }
                events.push(PtyEvent::Error { pty, code, message });
            }
        }
    }

    fn handle_pty_output(&mut self, pane: SessionId, bytes: Vec<u8>, events: &mut Vec<PtyEvent>) {
        let Some(pty) = self.key_for_pane(pane) else {
            return;
        };
        self.feed_pty_output(pty, &bytes, true);
        events.push(PtyEvent::Output {
            pty,
            byte_count: bytes.len(),
        });
    }

    /// The terminal a pane id belongs to, or `None` when we have no mapping for
    /// it.
    ///
    /// Only `Attach` establishes a mapping. Synthesising one from the id (the
    /// old behaviour) meant late output for a PTY that `stop`, `remove_pty` or
    /// `PaneExited` had just dropped resurrected a
    /// 5000-line scrollback parser that nothing would ever reclaim, deleted
    /// content reappeared on screen, and a rogue or stale daemon could exhaust
    /// client memory just by streaming output for distinct pane ids. Output for
    /// an unmapped pane is dropped instead.
    fn key_for_pane(&self, pane: SessionId) -> Option<PtyKey> {
        self.pane_index.get(&pane).copied()
    }

    fn record_foreground_process(&mut self, pty: PtyKey, process: ForegroundProcessInfo) {
        let foreground_is_child = matches!(
            (process.root_pid, process.foreground_pid),
            (Some(root_pid), Some(foreground_pid)) if root_pid != foreground_pid
        );
        if foreground_is_child {
            if let Some(command) = process.command.as_deref() {
                self.pane_mut(pty).commands.record_process_command(command);
            }
        }
        self.pane_mut(pty).foreground = Some(process);
    }

    /// Make sure a connection attempt is under way, without waiting for it.
    ///
    /// Returns `NotConnected` while there is no connection, which every caller
    /// already treats as "this message did not go out". Blocking here is what
    /// made a keystroke cost seconds (B6).
    fn ensure_connected(&mut self) -> PtyResult<()> {
        if self.connection.is_some() {
            return Ok(());
        }
        self.begin_connect(SpawnPolicy::Autospawn);
        Err(PtyError::NotConnected)
    }

    /// The daemon is unreachable. Retire every terminal we were tracking so they
    /// stop appearing live: record a connection-lost exit, drop the attachment
    /// mappings (so `is_running` becomes false and the app can restart them),
    /// and emit an exit event for each. Spawns still queued for a connection
    /// that never arrived are retired the same way.
    fn connection_lost(&mut self) {
        let ptys: Vec<PtyKey> = self
            .attached_ptys()
            .chain(self.pending_spawns.iter().map(|spawn| spawn.pty))
            .collect();
        self.pending_spawns.clear();
        for pty in ptys {
            self.detach(pty);
            let status = PtyExit {
                code: 1,
                signal: Some("mult-server connection lost".to_string()),
            };
            self.pane_mut(pty).exit = Some(status.clone());
            self.pending_events.push(PtyEvent::Exited { pty, status });
        }
    }

    /// Re-attach every terminal we still believe is running after establishing a
    /// fresh connection. The daemon's sessions outlive a dropped connection, so
    /// this restores live output without re-spawning anything. The server
    /// replays full scrollback on attach, so each parser is reset first to
    /// rebuild cleanly from the authoritative history rather than appending to
    /// stale content. A session the server no longer has answers with
    /// `PaneExited`, which surfaces as a normal exit and clears the terminal.
    fn reattach_ptys(&mut self) {
        let ptys: Vec<PtyKey> = self
            .attached_ptys()
            .filter(|pty| Some(*pty) != self.starting)
            .collect();
        for pty in ptys {
            let Ok(session) = session_for_key(pty) else {
                continue;
            };
            let size = self.parser_dimensions(pty);
            self.reset_parser(pty, size);
            let _ = self.write(&ClientMessage::Attach {
                session,
                rows: size.rows(),
                cols: size.cols(),
            });
        }
    }

    /// Every PTY currently attached to a daemon session.
    fn attached_ptys(&self) -> impl Iterator<Item = PtyKey> + '_ {
        self.panes
            .iter()
            .filter(|(_, pane)| pane.session.is_some())
            .map(|(pty, _)| *pty)
    }

    fn parser_dimensions(&self, pty: PtyKey) -> PtyDimensions {
        self.parser(pty)
            .map(|parser| {
                let (rows, cols) = parser.screen().size();
                PtyDimensions::new(rows, cols)
            })
            .unwrap_or_default()
    }

    /// Drop the current connection *and* shut its socket down, so the reader
    /// thread wakes, sees EOF and exits instead of staying parked on a live fd.
    fn close_connection(&mut self) {
        // `ServerConnection`'s `Drop` performs the shutdown; taking it here
        // makes the point of the drop explicit.
        drop(self.connection.take());
    }

    fn send(&mut self, message: ClientMessage) -> PtyResult<()> {
        self.ensure_connected()?;
        match self.write(&message) {
            Ok(()) => Ok(()),
            // Classification by `io::ErrorKind`, not by message text: these are
            // real socket errors from the OS, and the kind is what carries the
            // meaning.
            Err(PtyError::Io(error)) if is_disconnected_error(&error) => {
                // The socket is broken. Drop it and start a fresh connection in
                // the background; this message is lost, which is what a dropped
                // connection means. `drain_events` re-attaches or retires.
                self.close_connection();
                self.begin_connect(SpawnPolicy::Autospawn);
                Err(PtyError::Disconnected(error))
            }
            Err(PtyError::Io(error)) if is_write_stall(&error) => {
                // The daemon stopped reading long enough for a frame to be cut
                // in half, so the stream is no longer parseable. Nothing can be
                // reused: tear it down and reconnect (A2's last resort).
                self.close_connection();
                self.begin_connect(SpawnPolicy::Autospawn);
                Err(PtyError::WriteStalled(SERVER_WRITE_TIMEOUT))
            }
            Err(error) => Err(error),
        }
    }

    fn write(&mut self, message: &ClientMessage) -> PtyResult<()> {
        self.last_write = Instant::now();
        self.write_inner(message)
    }

    fn write_inner(&self, message: &ClientMessage) -> PtyResult<()> {
        let Some(connection) = &self.connection else {
            return Err(PtyError::NotConnected);
        };
        let mut writer = connection
            .writer
            .lock()
            .map_err(|_| PtyError::WriterPoisoned)?;
        write_message(&mut *writer, message).map_err(PtyError::Io)
    }
}

impl Drop for PtyRuntime {
    fn drop(&mut self) {
        if let Some(connection) = &self.connection {
            if let Ok(mut writer) = connection.writer.lock() {
                let _ = write_message(&mut *writer, &ClientMessage::Detach);
            }
        }
    }
}

impl PtyExit {
    pub fn label(&self) -> String {
        match &self.signal {
            Some(signal) => format!("terminated by {signal}"),
            None => format!("exit {}", self.code),
        }
    }
}

/// Strip everything the emulator would treat as a command from a status line.
///
/// Tabs become a space (they are formatting, not control) and every other
/// control character — C0, DEL and the C1 range, which includes the 8-bit
/// forms of CSI and OSC — is replaced by the replacement character, so the text
/// occupies exactly the cells it appears to.
fn sanitize_system_line(message: &str) -> String {
    message
        .chars()
        .map(|ch| match ch {
            '\t' => ' ',
            ch if ch.is_control() => char::REPLACEMENT_CHARACTER,
            ch => ch,
        })
        .collect()
}

fn terminal_screen_rows(parser: &Parser) -> Vec<String> {
    let (_, cols) = parser.screen().size();
    parser
        .screen()
        .rows(0, cols)
        .map(|row| row.trim_end().to_string())
        .collect()
}

fn clamp_parser_scrollback(parser: &mut Parser) {
    // Re-apply the current offset so vt100 clamps it to the available
    // in-memory scrollback after resizes, screen switches, or history trims.
    let current = parser.screen().scrollback();
    parser.set_scrollback(current);
}

/// Encode a single mouse event for a program that has enabled mouse
/// reporting. `button` is the xterm button byte; `col`/`row` are 1-based,
/// screen-relative cell coordinates.
fn encode_mouse_event(encoding: MouseProtocolEncoding, button: u8, col: u16, row: u16) -> Vec<u8> {
    match encoding {
        // SGR (DECSET 1006): `ESC [ < b ; x ; y M`. Coordinates are unbounded
        // here, and a wheel notch is always a press, so terminate with `M`.
        MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{col};{row}M").into_bytes(),
        // UTF-8 (1005): `ESC [ M` then button and coordinates as `value + 32`,
        // each written as a UTF-8 code point.
        MouseProtocolEncoding::Utf8 => {
            let mut bytes = b"\x1b[M".to_vec();
            bytes.push(button.wrapping_add(32));
            push_utf8_mouse_coord(&mut bytes, col);
            push_utf8_mouse_coord(&mut bytes, row);
            bytes
        }
        // X10/Default: one byte per field as `value + 32`, so anything past
        // column/row 223 cannot be represented — clamp rather than wrap.
        MouseProtocolEncoding::Default => {
            let mut bytes = b"\x1b[M".to_vec();
            bytes.push(button.wrapping_add(32));
            bytes.push(default_mouse_coord(col));
            bytes.push(default_mouse_coord(row));
            bytes
        }
    }
}

fn default_mouse_coord(coord: u16) -> u8 {
    (coord.min(223) as u8).wrapping_add(32)
}

fn push_utf8_mouse_coord(bytes: &mut Vec<u8>, coord: u16) {
    match char::from_u32(u32::from(coord) + 32) {
        Some(ch) => {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        None => bytes.push(b' '),
    }
}

fn terminal_paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }

    let mut bytes =
        Vec::with_capacity(BRACKETED_PASTE_START.len() + text.len() + BRACKETED_PASTE_END.len());
    bytes.extend_from_slice(BRACKETED_PASTE_START);
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(BRACKETED_PASTE_END);
    bytes
}

impl TerminalCommandTracker {
    fn record_input(&mut self, bytes: &[u8]) {
        for byte in bytes {
            let state = std::mem::take(&mut self.state);
            self.state = match state {
                TerminalInputTrackState::Ground => match *byte {
                    b'\r' | b'\n' => {
                        self.commit_input();
                        TerminalInputTrackState::Ground
                    }
                    0x03 | 0x15 => {
                        self.input.clear();
                        TerminalInputTrackState::Ground
                    }
                    0x08 | 0x7f => {
                        self.input.pop();
                        TerminalInputTrackState::Ground
                    }
                    0x1b => TerminalInputTrackState::Escape,
                    0x20..=0x7e => {
                        self.input.push(*byte as char);
                        TerminalInputTrackState::Ground
                    }
                    _ => TerminalInputTrackState::Ground,
                },
                TerminalInputTrackState::Escape => match *byte {
                    b'[' => TerminalInputTrackState::Csi,
                    _ => TerminalInputTrackState::Ground,
                },
                TerminalInputTrackState::Csi => {
                    if (0x40..=0x7e).contains(byte) {
                        TerminalInputTrackState::Ground
                    } else {
                        TerminalInputTrackState::Csi
                    }
                }
            };
        }
    }

    fn last_command(&self) -> Option<&str> {
        self.last.as_deref()
    }

    fn record_process_command(&mut self, command: &str) {
        let command = command.trim();
        if !command.is_empty() {
            self.last = Some(command.to_string());
        }
        self.input.clear();
    }

    fn commit_input(&mut self) {
        let command = self.input.trim();
        if !command.is_empty() {
            self.last = Some(command.to_string());
        }
        self.input.clear();
    }
}

impl TerminalResponseDetector {
    fn is_ground(&self) -> bool {
        matches!(self.state, TerminalResponseState::Ground)
    }

    /// Step the detector over one byte, reporting any query the byte completed.
    ///
    /// The screen is deliberately not an argument: a query's *reply* depends on
    /// screen state (the cursor position), but recognising the query does not.
    /// Keeping the two apart is what lets the caller scan a whole escape
    /// sequence ahead of the parser and still answer from the screen state at
    /// the exact query point.
    fn advance(&mut self, byte: u8) -> Option<TerminalQuery> {
        let state = std::mem::take(&mut self.state);
        let (next, response) = match state {
            TerminalResponseState::Ground => match byte {
                0x1b => (TerminalResponseState::Escape, None),
                _ => (TerminalResponseState::Ground, None),
            },
            TerminalResponseState::Escape => match byte {
                b'[' => {
                    self.csi_len = 0;
                    (TerminalResponseState::Csi, None)
                }
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    (TerminalResponseState::String { esc_seen: false }, None)
                }
                b'(' | b')' | b'*' | b'+' => (TerminalResponseState::IgnoreOne, None),
                b'Z' => (
                    TerminalResponseState::Ground,
                    Some(TerminalQuery::DeviceAttributes),
                ),
                _ => (TerminalResponseState::Ground, None),
            },
            TerminalResponseState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    let response = csi_terminal_query(&self.csi[..self.csi_len], byte as char);
                    self.csi_len = 0;
                    (TerminalResponseState::Ground, response)
                } else if self.csi_len >= TERMINAL_MAX_CSI_SEQUENCE_BYTES {
                    self.csi_len = 0;
                    (TerminalResponseState::CsiIgnored, None)
                } else {
                    self.csi[self.csi_len] = byte;
                    self.csi_len += 1;
                    (TerminalResponseState::Csi, None)
                }
            }
            TerminalResponseState::CsiIgnored => {
                if (0x40..=0x7e).contains(&byte) {
                    (TerminalResponseState::Ground, None)
                } else {
                    (TerminalResponseState::CsiIgnored, None)
                }
            }
            TerminalResponseState::String { esc_seen } => match (esc_seen, byte) {
                (_, 0x07) | (true, b'\\') => (TerminalResponseState::Ground, None),
                (_, 0x1b) => (TerminalResponseState::String { esc_seen: true }, None),
                _ => (TerminalResponseState::String { esc_seen: false }, None),
            },
            TerminalResponseState::IgnoreOne => (TerminalResponseState::Ground, None),
        };
        self.state = next;
        response
    }
}

/// Feed `bytes` to `parser` while letting `responder` answer terminal queries,
/// returning the reply bytes this chunk earned (concatenated, so the caller
/// makes at most one write out of them).
///
/// The parser is fed in the largest slices the responder allows, because a
/// one-byte `process` call pays vte's full dispatch setup for a single byte and
/// escape-dense output is the common case here:
///
/// * while the responder is idle no query can begin before the next ESC, so the
///   whole run up to it goes in one call;
/// * inside an escape sequence the responder is stepped ahead of the parser —
///   recognising a query needs no screen state — and the bytes it walked are
///   then fed in one call, up to and including the byte that completed a query.
///
/// A reply is generated from the screen *after* that final byte reaches the
/// parser, which is exactly where a byte-at-a-time feed would generate it, so
/// this stays behaviourally identical to feeding every byte individually
/// (`batched_feed_matches_per_byte_feed` in the tests pins that down).
fn feed_parser_with_responder(
    parser: &mut Parser,
    responder: &mut TerminalResponseDetector,
    budget: &mut TerminalResponseBudget,
    bytes: &[u8],
) -> Vec<u8> {
    let mut responses = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if responder.is_ground() {
            let run_end = bytes[index..]
                .iter()
                .position(|&byte| byte == 0x1b)
                .map_or(bytes.len(), |offset| index + offset);
            if run_end > index {
                parser.process(&bytes[index..run_end]);
                index = run_end;
                continue;
            }
        }

        // Walk the responder to the end of this escape sequence (or to the end
        // of the chunk, or to the query that terminates it) without touching
        // the parser, then hand the parser the whole span at once.
        let mut end = index;
        let mut query = None;
        while end < bytes.len() {
            let found = responder.advance(bytes[end]);
            end += 1;
            if found.is_some() || responder.is_ground() {
                query = found;
                break;
            }
        }
        parser.process(&bytes[index..end]);
        if let Some(query) = query {
            budget.push_response(query, parser.screen(), &mut responses);
        }
        index = end;
    }
    responses
}

/// Fuzzing seam (G3): drive the terminal-response path over `bytes` and return
/// the reply bytes it produced.
///
/// This is exactly the work [`PtyRuntime`] does for one chunk of PTY output,
/// minus the pane bookkeeping — a fresh parser, a fresh detector, and a fresh
/// per-chunk budget through [`feed_parser_with_responder`]. It exists so
/// `fuzz/fuzz_targets/vt_response_detector.rs` can reach a path that is
/// otherwise private, and so the fuzzer sees the same batching the runtime uses
/// rather than a byte-at-a-time reimplementation of it.
///
/// Not part of the supported API; it is hidden from the docs and may change or
/// disappear with the internals it wraps.
#[doc(hidden)]
pub fn fuzz_feed_terminal_responses(rows: u16, cols: u16, bytes: &[u8]) -> Vec<u8> {
    // Through `PtyDimensions` like every other screen, so the fuzzer exercises
    // the real clamp (A13) instead of a private one that could drift from it —
    // and so a target that asks for a 1×1 pane sees what a 1×1 pane actually
    // gets rather than an upstream panic.
    let size = PtyDimensions::new(rows, cols);
    let mut parser = Parser::new(size.rows(), size.cols(), 0);
    let mut detector = TerminalResponseDetector::default();
    let mut budget = TerminalResponseBudget::default();
    feed_parser_with_responder(&mut parser, &mut detector, &mut budget, bytes)
}

/// Classify a complete CSI sequence as a query this emulator answers.
fn csi_terminal_query(sequence: &[u8], final_char: char) -> Option<TerminalQuery> {
    let private = sequence.contains(&b'?');
    let params = parse_csi_params(sequence);
    match final_char {
        'c' if !private && param_or_default(&params, 0, 0) == 0 => {
            Some(TerminalQuery::DeviceAttributes)
        }
        'n' if !private => match param_or_default(&params, 0, 0) {
            5 => Some(TerminalQuery::DeviceStatus),
            6 => Some(TerminalQuery::CursorPosition { private: false }),
            _ => None,
        },
        'n' if private && param_or_default(&params, 0, 0) == 6 => {
            Some(TerminalQuery::CursorPosition { private: true })
        }
        _ => None,
    }
}

fn cursor_position_report(screen: &vt100::Screen, private: bool) -> Vec<u8> {
    let (row, col) = screen.cursor_position();
    if private {
        format!("\x1b[?{};{}R", row + 1, col + 1).into_bytes()
    } else {
        format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()
    }
}

fn parse_csi_params(sequence: &[u8]) -> Vec<usize> {
    String::from_utf8_lossy(sequence)
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

fn validate_server_hello_with_timeout(
    stream: &mut UnixStream,
    timeout: Duration,
) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    let result = validate_server_hello(stream);
    let reset_result = stream.set_read_timeout(None);

    if let Err(error) = result {
        return Err(map_server_hello_error(error, timeout));
    }

    reset_result
}

fn map_server_hello_error(error: io::Error, timeout: Duration) -> io::Error {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out after {timeout:?} waiting for mult-server hello"),
        )
    } else {
        error
    }
}

/// Check the daemon's hello.
///
/// The failures are typed as `PtyError` and converted at the return: this runs
/// on the connect worker, whose channel carries `io::Result` because everything
/// else on that path (connect, spawn, peer check) genuinely is I/O.
fn validate_server_hello(reader: &mut impl io::Read) -> io::Result<()> {
    let result = match read_message::<ServerMessage>(reader) {
        Ok(ServerMessage::Hello { protocol_version }) if protocol_version == PROTOCOL_VERSION => {
            return Ok(())
        }
        Ok(ServerMessage::Hello { protocol_version }) => PtyError::ProtocolMismatch {
            server: protocol_version,
        },
        Ok(ServerMessage::Error { code, message, .. }) => PtyError::Rejected { code, message },
        Ok(message) => PtyError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected mult-server hello response: {message:?}"),
        )),
        Err(error) => return Err(error),
    };
    Err(io::Error::from(result))
}

/// Build a fully-established connection: connect (autospawning the daemon if
/// `policy` allows it), verify the peer, exchange the protocol hello, and start the
/// reader thread that feeds `ServerMessage`s to the runtime.
///
/// Every blocking step lives here, in one function, precisely so it can be run
/// on a worker thread (B6). The caller adopts the result — or reports the
/// failure — on whichever frame it happens to arrive.
fn establish_connection(
    socket_path: &Path,
    instance: InstanceId,
    policy: SpawnPolicy,
) -> io::Result<EstablishedConnection> {
    let mut stream = match policy {
        SpawnPolicy::Autospawn => connect_or_spawn_server(socket_path)?,
        SpawnPolicy::ConnectOnly => UnixStream::connect(socket_path)?,
    };
    verify_peer_is_self(&stream, "mult-server")?;
    stream.set_nonblocking(false)?;
    // A bound on how long a write from the render thread may stall; see
    // `SERVER_WRITE_TIMEOUT`.
    stream.set_write_timeout(Some(SERVER_WRITE_TIMEOUT))?;
    let shutdown_handle = stream.try_clone()?;
    let mut writer_stream = stream.try_clone()?;
    write_message(
        &mut writer_stream,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            instance,
        },
    )?;
    validate_server_hello_with_timeout(&mut stream, SERVER_HELLO_TIMEOUT)?;

    let (sender, receiver) = mpsc::sync_channel(SERVER_EVENT_QUEUE_CAPACITY);
    thread::spawn(move || {
        let mut reader = stream;
        loop {
            match read_message::<ServerMessage>(&mut reader) {
                Ok(message) => {
                    if sender.send(message).is_err() {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    break;
                }
                Err(error) => {
                    let _ = sender.send(ServerMessage::Error {
                        pane: None,
                        code: RejectCode::Unspecified,
                        message: format!("failed to read from mult-server: {error}"),
                    });
                    break;
                }
            }
        }
    });

    Ok(EstablishedConnection {
        writer: writer_stream,
        socket: shutdown_handle,
        receiver,
    })
}

fn connect_or_spawn_server(path: &Path) -> io::Result<UnixStream> {
    match UnixStream::connect(path) {
        Ok(stream) => Ok(stream),
        Err(error) if should_autospawn_server(&error, path) => {
            spawn_server(path)?;
            wait_for_server(path).map_err(|wait_error| {
                io::Error::new(
                    wait_error.kind(),
                    format!(
                        "failed to connect to mult-server after autospawn: {wait_error}; initial error: {error}"
                    ),
                )
            })
        }
        Err(error) => Err(error),
    }
}

fn should_autospawn_server(error: &io::Error, path: &Path) -> bool {
    socket_connect_error_allows_autospawn(error, path)
        && autospawn_enabled()
        && server_executable().is_some()
}

fn socket_connect_error_allows_autospawn(error: &io::Error, path: &Path) -> bool {
    match error.kind() {
        io::ErrorKind::NotFound => true,
        io::ErrorKind::ConnectionRefused => path_is_socket(path),
        _ => false,
    }
}

fn path_is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;

    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
}

fn autospawn_enabled() -> bool {
    !matches!(
        env::var("MULT_SERVER_AUTOSPAWN").as_deref(),
        Ok("0") | Ok("false") | Ok("False") | Ok("FALSE")
    )
}

fn spawn_server(socket_path: &Path) -> io::Result<()> {
    let server = server_executable().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate mult-server next to the mult executable; run `mult-server` manually",
        )
    })?;

    let mut command = Command::new(server);
    apply_server_environment(&mut command);
    command
        .env(SOCKET_PATH_ENV, socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_autospawned_server(&mut command);
    command.spawn().map(|_| ())
}

/// Environment variables an autospawned daemon inherits from this client.
///
/// Everything else is dropped, and the reason is lifetime, not secrecy alone:
/// the daemon outlives the client that spawned it, and the environment it is
/// born with becomes the base environment of *every* PTY it ever spawns — for
/// every later client, every workspace, every terminal. Inheriting the full
/// environment therefore froze the first client's `ANTHROPIC_API_KEY`,
/// `AWS_*`, `SSH_AUTH_SOCK` and friends into every shell started days later,
/// from any project. These names are the ones a shell genuinely needs to be
/// usable; anything a workspace actually wants is set explicitly per session
/// through `PtySpawn::env`.
const INHERITED_SERVER_ENV_VARS: &[&str] = &["PATH", "HOME", "SHELL", "USER", "LOGNAME", "TERM"];

/// Prefixes kept alongside [`INHERITED_SERVER_ENV_VARS`]: locale settings, and
/// `MULT_*` because the daemon's own configuration travels that way.
const INHERITED_SERVER_ENV_PREFIXES: &[&str] = &["LC_", "MULT_"];

fn apply_server_environment(command: &mut Command) {
    command.env_clear();
    for (key, value) in env::vars_os() {
        let Some(name) = key.to_str() else {
            continue;
        };
        if server_env_var_is_inherited(name) {
            command.env(&key, value);
        }
    }
}

fn server_env_var_is_inherited(name: &str) -> bool {
    name == "LANG"
        || INHERITED_SERVER_ENV_VARS.contains(&name)
        || INHERITED_SERVER_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

fn detach_autospawned_server(command: &mut Command) {
    // Autospawned servers should behave like a small user daemon: if the
    // terminal running the `mult` client is closed, the server must not receive
    // that terminal's hangup and tear down the PTYs it owns.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Poll a just-spawned daemon's socket until it accepts connections.
///
/// The deadline is a ceiling for a daemon that never comes up, not an expected
/// wait: the loop returns on the first successful `connect`, so a healthy spawn
/// costs one poll interval. Two seconds was tight enough that a loaded machine
/// (or a cold binary being paged in) lost the race and the client reported the
/// daemon as unreachable when it was merely slow.
fn wait_for_server(path: &Path) -> io::Result<UnixStream> {
    let deadline = Instant::now() + SERVER_SPAWN_TIMEOUT;
    let mut last_error = None;
    while Instant::now() < deadline {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_millis(25));
    }

    Err(last_error.unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "timed out")))
}

/// The `mult-server` binary next to this executable, if it exists and is safe
/// to exec.
///
/// Autospawn resolves the daemon purely by filename convention, so the file it
/// finds is only as trustworthy as the directory `mult` itself lives in.
/// Anything writable by group or others — or owned by another non-root user —
/// would turn "run `mult`" into "run whatever they dropped next to it", as this
/// user, in a process that then outlives the session. [`server_binary_is_safe`]
/// is what rules that out; a rejected binary simply means no autospawn, and the
/// user is told to start `mult-server` themselves.
fn server_executable() -> Option<PathBuf> {
    let mut path = env::current_exe().ok()?;
    let stem = path.file_stem()?.to_str()?;
    if stem != "mult" {
        return None;
    }

    path.set_file_name(server_executable_name());
    let metadata = fs::symlink_metadata(&path).ok()?;
    server_binary_is_safe(&metadata).then_some(path)
}

/// Whether a resolved daemon binary may be executed.
///
/// Owned by this user or by root (root can already do anything, and a
/// system-installed `mult-server` is root-owned by design), a regular file, and
/// with no group/other write bit. Symlinks are rejected outright — the metadata
/// comes from `symlink_metadata`, so a link's own type fails the regular-file
/// test rather than being followed to a target whose mode says nothing about
/// who can retarget the link.
#[cfg(unix)]
fn server_binary_is_safe(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    if !metadata.file_type().is_file() {
        return false;
    }
    let owner = metadata.uid();
    if owner != mult_protocol::peer::effective_uid() && owner != 0 {
        return false;
    }
    metadata.mode() & 0o022 == 0
}

#[cfg(not(unix))]
fn server_binary_is_safe(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_file()
}

fn server_executable_name() -> &'static str {
    if cfg!(windows) {
        "mult-server.exe"
    } else {
        "mult-server"
    }
}

/// A socket write that hit [`SERVER_WRITE_TIMEOUT`]. The kind differs by
/// platform (`WouldBlock` on Linux, `TimedOut` elsewhere).
fn is_write_stall(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn is_disconnected_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
    )
}

/// The daemon-facing id for a PTY key, or the error a caller reports when the
/// key holds an id no session can have. The encoding itself now lives in
/// `mult_protocol` (F4); this is only the failure message.
fn session_for_key(key: PtyKey) -> PtyResult<SessionId> {
    key.wire_id().ok_or(PtyError::UnroutableKey(key))
}

fn launch_spec(spawn: &PtySpawn) -> LaunchSpec {
    spawn
        .args
        .last()
        .cloned()
        .map(LaunchSpec::Command)
        .unwrap_or(LaunchSpec::Shell)
}

fn session_name(session: SessionId, launch: &LaunchSpec) -> String {
    match launch {
        LaunchSpec::Shell => format!("shell {}", session.0),
        LaunchSpec::Command(command) => command.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::net::UnixListener,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::model::TerminalId;

    #[test]
    fn pty_spawn_uses_default_size() {
        let spawn = PtySpawn::shell(
            PtyKey::Terminal(TerminalId::new(7).unwrap()),
            None,
            BTreeMap::new(),
        );

        assert_eq!(spawn.pty, PtyKey::Terminal(TerminalId::new(7).unwrap()));
        assert_eq!(spawn.args, Vec::<String>::new());
        assert_eq!(spawn.size, PtyDimensions::new(24, 80));
        assert!(!spawn.program.is_empty());
    }

    #[test]
    fn pty_spawn_command_line_runs_through_shell() {
        let spawn = PtySpawn::command_line(
            PtyKey::Terminal(TerminalId::new(7).unwrap()),
            "cargo test".to_string(),
            None,
            BTreeMap::new(),
        );

        assert_eq!(spawn.pty, PtyKey::Terminal(TerminalId::new(7).unwrap()));
        assert_eq!(spawn.args.last().map(String::as_str), Some("cargo test"));
        assert!(!spawn.program.is_empty());
    }

    #[test]
    fn pty_exit_has_human_label() {
        let exit = PtyExit {
            code: 2,
            signal: None,
        };

        assert_eq!(exit.label(), "exit 2");
    }

    #[test]
    fn parser_processes_output_and_preserves_scrollback_cap() {
        let mut runtime = PtyRuntime::new_offline();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));
        runtime.process_pty_output(pty, b"one\r\ntwo\r\nthree");

        assert_eq!(
            runtime.pty_lines(pty),
            vec!["two".to_string(), "three".to_string()]
        );
        assert!(runtime.parser(pty).is_some());
        assert!(!runtime.pty_output_is_blank(pty));
    }

    #[test]
    fn parser_resize_updates_screen_size() {
        let mut runtime = PtyRuntime::new_offline();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));

        runtime
            .resize(pty, PtyDimensions::new(5, 12))
            .expect("resize parser");

        assert_eq!(runtime.parser(pty).unwrap().screen().size(), (5, 12));
    }

    /// F12: a resize is not a way to bring a pane into existence.
    ///
    /// `resize_visible_terminal` runs before the layout is recomputed, so for
    /// one tick after a delete it still names the removed terminal — and
    /// `pane_mut`'s `or_default` then allocated a fresh 5000-line screen for a
    /// `TerminalId` that no longer exists and can never be selected again. One
    /// leaked buffer per deleted pane, for the life of the session.
    #[test]
    fn resizing_a_pane_that_does_not_exist_creates_nothing() {
        let mut runtime = PtyRuntime::new_offline();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));
        assert!(runtime.has_pane(pty));

        runtime.remove_pty(pty);
        runtime
            .resize(pty, PtyDimensions::new(40, 120))
            .expect("resizing a deleted pane is not an error");

        assert!(!runtime.has_pane(pty));
        assert!(runtime.parser(pty).is_none());
    }

    /// A13. Every byte class the upstream overflow was reachable with, at every
    /// grid the layout can produce below and at the floor, through both paths
    /// that build a screen (`reset_parser` → `Parser::new`, `resize` →
    /// `set_size`). Without the clamp this panics with "attempt to subtract with
    /// overflow" — in debug, and with the terminal in raw mode.
    #[test]
    fn one_row_and_one_column_panes_are_clamped_to_a_size_the_emulator_survives() {
        // A wrapped ASCII line reaches `grid.rs:637`; a double-width character
        // reaches `screen.rs:788`; a lone continuation byte is the shortest
        // input that produced the second one.
        let inputs: [&[u8]; 5] = [
            b"\x80",
            "\u{1f600}".as_bytes(),
            "\u{6f22}\u{5b57}".as_bytes(),
            b"hello world, wide enough to wrap",
            b"a\r\nb\r\nc\r\n",
        ];

        for (rows, cols) in [(0, 0), (1, 1), (1, 40), (40, 1), (2, 2)] {
            for bytes in inputs {
                let mut runtime = PtyRuntime::new_offline();
                let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());

                runtime.reset_parser(pty, PtyDimensions::new(rows, cols));
                runtime.process_pty_output(pty, bytes);
                runtime
                    .resize(pty, PtyDimensions::new(rows, cols))
                    .expect("resize an offline pane");
                runtime.process_pty_output(pty, bytes);

                let size = runtime
                    .parser(pty)
                    .expect("pane has a screen")
                    .screen()
                    .size();
                assert_eq!(
                    size,
                    (rows.max(MIN_PTY_ROWS), cols.max(MIN_PTY_COLS)),
                    "a {rows}x{cols} pane must be raised to the emulator's floor"
                );
            }
        }
    }

    /// The clamp is on the type, so no call site can opt out of it — including
    /// the one the daemon is told about, which must match the screen the client
    /// parses or a redraw comes out the wrong shape.
    #[test]
    fn pty_dimensions_cannot_be_built_below_the_emulator_floor() {
        assert_eq!(
            (
                PtyDimensions::new(0, 0).rows(),
                PtyDimensions::new(0, 0).cols()
            ),
            (MIN_PTY_ROWS, MIN_PTY_COLS)
        );
        assert_eq!(PtyDimensions::new(1, 200).rows(), MIN_PTY_ROWS);
        assert_eq!(PtyDimensions::new(200, 1).cols(), MIN_PTY_COLS);
        // Sizes at or above the floor are untouched.
        assert_eq!(PtyDimensions::new(2, 2), PtyDimensions::new(2, 2));
        assert_eq!(PtyDimensions::new(40, 120).rows(), 40);
        assert_eq!(PtyDimensions::new(40, 120).cols(), 120);
    }

    /// The fuzzing seam clamps through the same type, so `vt_response_detector`
    /// can be handed a 1×1 grid without rediscovering the upstream panic.
    #[test]
    fn the_fuzz_seam_clamps_its_dimensions_too() {
        for (rows, cols) in [(0, 0), (1, 1), (1, 80), (80, 1)] {
            let responses =
                fuzz_feed_terminal_responses(rows, cols, b"\x80\x1b[6n\xf0\x9f\x98\x80");
            assert!(!responses.is_empty(), "a cursor report is still answered");
        }
    }

    #[test]
    fn send_paste_wraps_when_parser_reports_bracketed_paste() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        let pane = SessionId(7);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));
        runtime.process_pty_output(pty, b"\x1b[?2004h");

        assert!(runtime.send_paste(pty, "one\ntwo").expect("paste"));

        let message: ClientMessage = read_message(&mut server_stream).expect("read paste input");
        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                bytes: b"\x1b[200~one\ntwo\x1b[201~".to_vec(),
            }
        );
    }

    #[test]
    fn validate_server_hello_rejects_incompatible_protocol_version() {
        let mut bytes = Vec::new();
        write_message(
            &mut bytes,
            &ServerMessage::Hello {
                protocol_version: PROTOCOL_VERSION + 1,
            },
        )
        .expect("write hello");

        let error = validate_server_hello(&mut bytes.as_slice()).expect_err("reject version");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("restart mult-server"));
    }

    #[test]
    fn validate_server_hello_times_out_when_peer_is_silent() {
        let (mut client, _server) = UnixStream::pair().expect("create socket pair");

        let error = validate_server_hello_with_timeout(&mut client, Duration::from_millis(10))
            .expect_err("silent peer should time out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("waiting for mult-server hello"));
    }

    #[test]
    fn server_supplied_text_cannot_paint_escape_sequences_into_a_pane() {
        // `ServerMessage::Error` text and `ExitInfo::signal` come from the
        // daemon. Fed raw to the parser they let it clear a pane, move the
        // cursor and forge its own `[mult]` line — UI spoofing inside mult.
        assert_eq!(
            sanitize_system_line("boom\x1b[2J\x1b[H[mult] all good"),
            "boom\u{fffd}[2J\u{fffd}[H[mult] all good"
        );
        // C1 CSI/OSC in their 8-bit forms are control characters too.
        assert_eq!(sanitize_system_line("a\u{9b}31mb"), "a\u{fffd}31mb");
        assert_eq!(
            sanitize_system_line("line\r\nline"),
            "line\u{fffd}\u{fffd}line"
        );
        assert_eq!(sanitize_system_line("col\tcol"), "col col");
        assert_eq!(sanitize_system_line("exit 0"), "exit 0");

        let pty = PtyKey::Terminal(TerminalId::new(11).unwrap());
        let mut runtime = PtyRuntime::new_offline();
        runtime.reset_parser(pty, PtyDimensions::new(3, 40));
        runtime.process_pty_output(pty, b"first line\r\n");
        runtime.append_pty_system_line(pty, "terminated by \x1b[2J\x1b[HSIGKILL");

        // The screen was not cleared and the cursor was not repositioned: the
        // earlier line survives and the status line lands under it.
        let lines = runtime.pty_lines(pty);
        assert_eq!(lines[0], "first line");
        assert!(lines[1].starts_with("[mult] terminated by "), "{lines:?}");
        assert!(!lines[1].contains('\u{1b}'), "{lines:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_group_writable_daemon_binary_is_not_autospawned() {
        use std::os::unix::fs::PermissionsExt;

        if mult_protocol::peer::effective_uid() == 0 {
            return;
        }

        let dir = unique_pty_test_dir("server-mode");
        let binary = dir.join("mult-server");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").expect("write fake daemon");

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("chmod 0755");
        let metadata = fs::symlink_metadata(&binary).expect("metadata");
        assert!(server_binary_is_safe(&metadata));

        // Anyone who can write next to `mult` could otherwise replace the
        // daemon and have autospawn exec it as this user.
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o775)).expect("chmod 0775");
        let metadata = fs::symlink_metadata(&binary).expect("metadata");
        assert!(!server_binary_is_safe(&metadata));

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o757)).expect("chmod 0757");
        let metadata = fs::symlink_metadata(&binary).expect("metadata");
        assert!(!server_binary_is_safe(&metadata));

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_daemon_binary_is_not_autospawned() {
        let dir = unique_pty_test_dir("server-symlink");
        let target = dir.join("real");
        fs::write(&target, b"#!/bin/sh\nexit 0\n").expect("write fake daemon");
        let link = dir.join("mult-server");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let metadata = fs::symlink_metadata(&link).expect("metadata");
        assert!(!server_binary_is_safe(&metadata));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_daemon_environment_keeps_only_what_a_shell_needs() {
        for kept in [
            "PATH",
            "HOME",
            "SHELL",
            "USER",
            "LOGNAME",
            "TERM",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "MULT_SOCKET_PATH",
            "MULT_SERVER_AUTOSPAWN",
        ] {
            assert!(server_env_var_is_inherited(kept), "{kept} must be kept");
        }

        // Credentials and session handles are exactly what must not be frozen
        // into a daemon that outlives this client and seeds every later PTY.
        for dropped in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "GPG_TTY",
            "LANGUAGE",
            "MULTIPASS",
        ] {
            assert!(
                !server_env_var_is_inherited(dropped),
                "{dropped} must be dropped"
            );
        }
    }

    #[cfg(unix)]
    fn unique_pty_test_dir(label: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = env::temp_dir().join(format!(
            "mult-pty-test-{label}-{}-{nanos}",
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
    fn the_client_verifies_peer_credentials_through_the_shared_check() {
        // The accept/reject rule itself is tested in `mult_protocol::peer`; what
        // matters here is that this binary is wired to it at all, since the
        // check used to be a private copy that could rot independently.
        let (client, _server) = UnixStream::pair().expect("create socket pair");

        verify_peer_is_self(&client, "test peer").expect("same uid peer is accepted");
    }

    #[test]
    fn start_rolls_back_local_attachment_when_attach_is_rejected() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::sync_channel(8);
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        let pane = SessionId(7);
        let mut runtime = test_runtime_without_attachments(client_stream, receiver);
        let server = thread::spawn(move || {
            let create: ClientMessage = read_message(&mut server_stream).expect("read create");
            assert!(matches!(
                create,
                ClientMessage::CreateSession {
                    requested_id: Some(SessionId(7)),
                    ..
                }
            ));
            let attach: ClientMessage = read_message(&mut server_stream).expect("read attach");
            assert_eq!(
                attach,
                ClientMessage::Attach {
                    session: SessionId(7),
                    rows: 24,
                    cols: 80,
                }
            );
            sender
                .send(ServerMessage::Error {
                    pane: Some(SessionId(7)),
                    code: RejectCode::SessionBusy,
                    message: "pane 7 was taken over by another mult client".to_string(),
                })
                .expect("send attach rejection");
        });

        let error = runtime
            .start(PtySpawn::shell(pty, None, BTreeMap::new()))
            .expect_err("attach rejection should fail start");

        // The reason comes off the wire as a `RejectCode`. It used to be
        // recovered by substring-matching the daemon's prose (F8), and Slice 9
        // had already reworded that text out from under the match.
        assert!(
            matches!(
                error,
                PtyError::Rejected {
                    code: RejectCode::SessionBusy,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(!runtime.is_running(pty));
        assert!(runtime.key_for_pane(pane).is_none());
        server.join().expect("server thread should finish");
    }

    #[test]
    fn pty_stop_sends_stop_message_and_clears_local_attachment() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        let pane = SessionId(7);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);

        assert!(runtime.stop(pty).expect("stop pty"));

        let message: ClientMessage = read_message(&mut server_stream).expect("read stop message");
        assert_eq!(message, ClientMessage::Stop { pane });
        assert!(!runtime.is_running(pty));
        assert!(runtime.key_for_pane(pane).is_none());
    }

    #[test]
    fn input_returns_scrolled_parser_to_bottom() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        let pane = SessionId(7);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));
        runtime.process_pty_output(pty, b"one\r\ntwo\r\nthree");
        assert!(runtime.scroll_up(pty, 1));
        assert!(runtime.parser(pty).unwrap().screen().scrollback() > 0);

        assert!(runtime.send_input(pty, b"x").expect("send input"));

        assert_eq!(runtime.parser(pty).unwrap().screen().scrollback(), 0);
        let message: ClientMessage = read_message(&mut server_stream).expect("read input");
        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                bytes: b"x".to_vec(),
            }
        );
    }

    #[test]
    fn encode_mouse_event_covers_each_protocol_encoding() {
        // SGR: human-readable decimal coordinates, terminated with `M`.
        assert_eq!(
            encode_mouse_event(MouseProtocolEncoding::Sgr, WHEEL_UP_BUTTON, 12, 5),
            b"\x1b[<64;12;5M".to_vec()
        );
        // X10/Default: one byte per field as `value + 32`, clamped at 223.
        assert_eq!(
            encode_mouse_event(MouseProtocolEncoding::Default, WHEEL_DOWN_BUTTON, 1, 1),
            vec![0x1b, b'[', b'M', 65 + 32, 1 + 32, 1 + 32]
        );
        assert_eq!(
            encode_mouse_event(MouseProtocolEncoding::Default, WHEEL_UP_BUTTON, 1000, 1),
            vec![0x1b, b'[', b'M', 64 + 32, 223 + 32, 1 + 32]
        );
        // UTF-8: `value + 32` written as a code point (multi-byte past 223).
        assert_eq!(
            encode_mouse_event(MouseProtocolEncoding::Utf8, WHEEL_UP_BUTTON, 300, 1),
            {
                let mut expected = vec![0x1b, b'[', b'M', 64 + 32];
                let mut buf = [0u8; 4];
                expected.extend_from_slice(
                    char::from_u32(300 + 32)
                        .unwrap()
                        .encode_utf8(&mut buf)
                        .as_bytes(),
                );
                expected.push(1 + 32);
                expected
            }
        );
    }

    #[test]
    fn wheel_is_forwarded_to_a_mouse_reporting_program() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        let pane = SessionId(7);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(24, 80));
        // Claude Code's startup: enter the alternate screen and request SGR
        // mouse reporting. After this the program owns the wheel.
        runtime.process_pty_output(pty, b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        assert!(runtime.pty_reports_mouse(pty));

        assert!(runtime.forward_wheel(pty, true, 12, 5));

        let message: ClientMessage =
            read_message(&mut server_stream).expect("read forwarded wheel");
        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                bytes: b"\x1b[<64;12;5M".to_vec(),
            }
        );
    }

    #[test]
    fn wheel_is_not_forwarded_when_the_program_ignores_the_mouse() {
        let mut runtime = PtyRuntime::new_offline();
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));
        runtime.process_pty_output(pty, b"one\r\ntwo\r\nthree");

        assert!(!runtime.pty_reports_mouse(pty));
        assert!(!runtime.forward_wheel(pty, true, 1, 1));
    }

    #[test]
    fn parser_scrolls_beyond_visible_screen_height() {
        let mut runtime = PtyRuntime::new_offline();
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));
        runtime.process_pty_output(pty, b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");

        assert!(runtime.scroll_up(pty, 4));

        assert_eq!(runtime.parser(pty).unwrap().screen().scrollback(), 4);
        assert_eq!(
            runtime.pty_lines(pty),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn pty_scroll_is_local_and_paste_sends_input_message() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        let pane = SessionId(7);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));
        runtime.process_pty_output(pty, b"one\r\ntwo\r\nthree");

        assert!(runtime.scroll_up(pty, 1));
        assert!(runtime.scroll_down(pty, 1));
        assert!(!runtime.scroll_up(PtyKey::Terminal(TerminalId::new(99).unwrap()), 1));
        assert!(runtime.send_paste(pty, "one\ntwo").expect("paste"));

        let message: ClientMessage = read_message(&mut server_stream).expect("read client message");
        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                bytes: b"one\ntwo".to_vec(),
            }
        );
    }

    #[test]
    fn pty_stop_keeps_local_attachment_when_send_fails() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        let pane = SessionId(7);
        let socket = client_stream.try_clone().expect("clone test socket");
        let writer = Arc::new(Mutex::new(client_stream));
        let poison_writer = writer.clone();
        let _ = thread::spawn(move || {
            let _guard = poison_writer.lock().expect("lock writer");
            panic!("poison writer lock");
        })
        .join();
        let mut runtime = test_runtime_from_connection(
            ServerConnection {
                writer,
                receiver,
                socket,
            },
            Some((pty, pane)),
        );

        let error = runtime.stop(pty).expect_err("stop should fail");

        assert!(matches!(error, PtyError::WriterPoisoned), "{error:?}");
        assert!(runtime.is_running(pty));
        assert_eq!(runtime.key_for_pane(pane), Some(pty));
    }

    #[test]
    fn pane_exit_event_clears_local_attachment() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);

        sender
            .send(ServerMessage::PaneExited {
                pane,
                exit: mult_protocol::ExitInfo {
                    code: 3,
                    signal: None,
                },
            })
            .expect("send exit event");

        let events = runtime.drain_events();

        assert_eq!(
            events,
            vec![PtyEvent::Exited {
                pty,
                status: PtyExit {
                    code: 3,
                    signal: None,
                },
            }]
        );
        assert!(!runtime.is_running(pty));
        assert!(runtime.key_for_pane(pane).is_none());
        assert_eq!(
            runtime.pty_exit_status(pty),
            Some(&PtyExit {
                code: 3,
                signal: None,
            })
        );
    }

    #[test]
    fn terminal_last_command_tracks_submitted_input() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);

        assert!(runtime
            .send_input(pty, b"cargo test")
            .expect("send command"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read command input");
        assert_eq!(runtime.pty_last_command(pty), None);

        assert!(runtime.send_input(pty, b"\r").expect("send enter"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read enter input");

        assert_eq!(runtime.pty_last_command(pty), Some("cargo test"));
    }

    #[test]
    fn terminal_last_command_ignores_fullscreen_app_input() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));

        assert!(runtime.send_input(pty, b"nvim\r").expect("send nvim"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read nvim input");
        assert_eq!(runtime.pty_last_command(pty), Some("nvim"));

        runtime.process_pty_output(pty, b"\x1b[?1049h");
        assert!(runtime
            .send_input(pty, b"asdasdq\r")
            .expect("send editor input"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read editor input");

        assert_eq!(runtime.pty_last_command(pty), Some("nvim"));
    }

    #[test]
    fn terminal_last_command_uses_foreground_process_not_child_input() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);

        sender
            .send(ServerMessage::ForegroundProcess {
                pane,
                process: ForegroundProcessInfo {
                    root_pid: Some(10),
                    foreground_pid: Some(20),
                    command: Some("python".to_string()),
                },
            })
            .expect("send foreground process");
        assert!(runtime.drain_events().is_empty());
        assert_eq!(runtime.pty_last_command(pty), Some("python"));

        assert!(runtime
            .send_input(pty, b"print('typed text')\r")
            .expect("send child input"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read child input");
        assert_eq!(runtime.pty_last_command(pty), Some("python"));

        sender
            .send(ServerMessage::ForegroundProcess {
                pane,
                process: ForegroundProcessInfo {
                    root_pid: Some(10),
                    foreground_pid: Some(10),
                    command: Some("bash".to_string()),
                },
            })
            .expect("send shell foreground process");
        assert!(runtime.drain_events().is_empty());
        assert!(runtime
            .send_input(pty, b"cargo test\r")
            .expect("send shell input"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read shell input");
        assert_eq!(runtime.pty_last_command(pty), Some("cargo test"));
    }

    #[test]
    fn live_output_answers_primary_device_attributes_query() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));

        sender
            .send(ServerMessage::PtyOutput {
                pane,
                bytes: b"\x1b[c".to_vec(),
            })
            .expect("send pty query");

        let events = runtime.drain_events();
        let message: ClientMessage = read_message(&mut server_stream).expect("read DA response");

        assert_eq!(events, vec![PtyEvent::Output { pty, byte_count: 3 }]);
        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                bytes: PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.to_vec(),
            }
        );
    }

    #[test]
    fn live_output_reports_cursor_after_a_batched_run() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));

        // "abc" advances the cursor to column 3 and is fed as a batched run; the
        // trailing DSR cursor-position query must still report the cursor at the
        // query point (row 1, col 4 in 1-based terms), proving the batched feed
        // matches byte-by-byte behaviour.
        sender
            .send(ServerMessage::PtyOutput {
                pane,
                bytes: b"abc\x1b[6n".to_vec(),
            })
            .expect("send output with embedded cursor query");

        let _ = runtime.drain_events();
        let message: ClientMessage = read_message(&mut server_stream).expect("read DSR response");

        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                bytes: b"\x1b[1;4R".to_vec(),
            }
        );
    }

    #[test]
    fn output_event_feeds_matching_parser() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));

        sender
            .send(ServerMessage::PtyOutput {
                pane,
                bytes: b"hello".to_vec(),
            })
            .expect("send output event");

        let events = runtime.drain_events();

        assert_eq!(events, vec![PtyEvent::Output { pty, byte_count: 5 }]);
        assert_eq!(runtime.pty_lines(pty)[0], "hello");
    }

    /// B6: connecting must not be paid for on the render thread.
    ///
    /// The daemon here accepts the connection and then says nothing, which is
    /// the worst case the old inline path had: it sat in the hello read for
    /// `SERVER_HELLO_TIMEOUT`, and a keystroke that triggered a reconnect paid
    /// that plus the spawn wait plus the attach acknowledgement.
    #[test]
    fn connecting_and_starting_never_block_the_caller() {
        let path = unique_socket_path();
        let listener = UnixListener::bind(&path).expect("bind a silent test daemon");
        // Detached on purpose: the test must not wait for it either.
        thread::spawn(move || {
            let accepted = listener.accept();
            thread::sleep(SERVER_HELLO_TIMEOUT * 2);
            drop(accepted);
        });

        let pty = PtyKey::Terminal(TerminalId::new(31).unwrap());
        let started = Instant::now();
        let mut runtime =
            PtyRuntime::with_socket_path(path.clone(), InstanceId(1), SpawnPolicy::ConnectOnly);
        let mut spawn = PtySpawn::shell(pty, None, BTreeMap::new());
        spawn.size = PtyDimensions::new(6, 40);
        runtime
            .start(spawn)
            .expect("a start with no connection queues");
        let elapsed = started.elapsed();

        assert!(
            elapsed < SERVER_HELLO_TIMEOUT,
            "connecting blocked the caller for {elapsed:?}"
        );
        // The queued spawn counts as running, so the app neither restarts it
        // every frame nor shows it as dead while the daemon is coming up.
        assert!(runtime.is_running(pty));

        drop(runtime);
        let _ = fs::remove_file(&path);
    }

    /// B6: a spawn queued against a daemon that never arrives is retired, not
    /// left pretending to run.
    #[test]
    fn a_spawn_queued_while_disconnected_is_retired_when_the_connection_fails() {
        // No socket at this path, and `ConnectOnly` forbids autospawn outright,
        // so the background connect fails quickly.
        let mut runtime = PtyRuntime::with_socket_path(
            unique_socket_path(),
            InstanceId(1),
            SpawnPolicy::ConnectOnly,
        );
        let pty = PtyKey::Terminal(TerminalId::new(32).unwrap());
        let mut spawn = PtySpawn::shell(pty, None, BTreeMap::new());
        spawn.size = PtyDimensions::new(6, 40);
        runtime.start(spawn).expect("queue the spawn");
        assert!(runtime.is_running(pty));

        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            events.extend(runtime.drain_events());
            if events
                .iter()
                .any(|event| matches!(event, PtyEvent::Exited { .. }))
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        assert!(
            events.iter().any(|event| matches!(
                event,
                PtyEvent::Error { pty: None, message, .. } if message.contains("failed to connect")
            )),
            "the connection failure must reach the status line: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            PtyEvent::Exited { pty: exited, .. } if *exited == pty
        )));
        assert!(!runtime.is_running(pty));
    }

    #[test]
    fn lost_connection_retires_ptys_when_reconnect_fails() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(10).unwrap());
        let pane = SessionId(10);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));
        drop(sender);

        // Reconnection is a background activity now (B6), so the retire lands on
        // whichever frame the failed attempt is collected on rather than on the
        // frame the connection dropped. Bounded so a regression fails the test
        // instead of hanging it.
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            events.extend(runtime.drain_events());
            if events
                .iter()
                .any(|event| matches!(event, PtyEvent::Exited { .. }))
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        // The test socket has no server and tests never autospawn, so the dropped
        // connection cannot be restored. The terminal is retired with a
        // connection-lost exit instead of being left to look like it is running,
        // while its parser (last output) is kept until a restart resets it.
        assert!(runtime.connection.is_none());
        assert!(!runtime.is_running(pty));
        assert!(runtime.pane_index.is_empty());
        assert!(runtime.parser(pty).is_some());
        assert!(events.iter().any(|event| matches!(
            event,
            PtyEvent::Exited { pty: exited, status }
                if *exited == pty
                    && status.signal.as_deref() == Some("mult-server connection lost")
        )));
    }

    #[test]
    fn reattach_ptys_reattaches_known_sessions_with_parser_dimensions() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(5).unwrap());
        let pane = session_for_key(pty).expect("test key has a session id");
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(6, 20));

        runtime.reattach_ptys();

        let message: ClientMessage = read_message(&mut server_stream).expect("read reattach");
        assert_eq!(
            message,
            ClientMessage::Attach {
                session: session_for_key(pty).expect("test key has a session id"),
                rows: 6,
                cols: 20,
            }
        );
    }

    #[test]
    fn reattach_skips_the_terminal_currently_starting() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(5).unwrap());
        let pane = session_for_key(pty).expect("test key has a session id");
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(6, 20));
        runtime.starting = Some(pty);

        runtime.reattach_ptys();

        // The terminal being started must not be re-attached, so nothing is sent.
        server_stream
            .set_nonblocking(true)
            .expect("set nonblocking");
        assert!(read_message::<ClientMessage>(&mut server_stream).is_err());
    }

    #[test]
    fn autospawn_is_allowed_for_missing_or_stale_socket_paths_only() {
        let missing = unique_socket_path();
        let missing_error = io::Error::new(io::ErrorKind::NotFound, "missing");
        assert!(socket_connect_error_allows_autospawn(
            &missing_error,
            &missing
        ));

        let stale = unique_socket_path();
        let listener = UnixListener::bind(&stale).expect("bind stale socket");
        drop(listener);
        let refused_error = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
        assert!(socket_connect_error_allows_autospawn(
            &refused_error,
            &stale
        ));
        fs::remove_file(&stale).expect("remove stale socket");

        let regular_file = unique_socket_path();
        fs::write(&regular_file, "not a socket").expect("write collision file");
        assert!(!socket_connect_error_allows_autospawn(
            &refused_error,
            &regular_file
        ));
        fs::remove_file(&regular_file).expect("remove collision file");
    }

    fn test_runtime(
        client_stream: UnixStream,
        receiver: Receiver<ServerMessage>,
        pty: PtyKey,
        pane: SessionId,
    ) -> PtyRuntime {
        test_runtime_from_connection(test_connection(client_stream, receiver), Some((pty, pane)))
    }

    fn test_runtime_without_attachments(
        client_stream: UnixStream,
        receiver: Receiver<ServerMessage>,
    ) -> PtyRuntime {
        test_runtime_from_connection(test_connection(client_stream, receiver), None)
    }

    fn test_connection(
        client_stream: UnixStream,
        receiver: Receiver<ServerMessage>,
    ) -> ServerConnection {
        let socket = client_stream.try_clone().expect("clone test socket");
        ServerConnection {
            writer: Arc::new(Mutex::new(client_stream)),
            receiver,
            socket,
        }
    }

    fn test_runtime_from_connection(
        connection: ServerConnection,
        attachment: Option<(PtyKey, SessionId)>,
    ) -> PtyRuntime {
        let mut runtime = PtyRuntime::disconnected(unique_socket_path(), InstanceId(1), Vec::new());
        runtime.connection = Some(connection);
        if let Some((pty, pane)) = attachment {
            runtime.attach(pty, pane);
        }
        runtime
    }

    fn unique_socket_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-pty-test-{unique}.sock"))
    }

    /// F2: one entry per PTY, so removal cannot forget a field.
    ///
    /// The eight parallel maps this replaced each had to be cleared by hand,
    /// and an omission left state behind that nothing would ever reclaim — a
    /// deleted terminal's scrollback, its recorded exit, its last command.
    #[test]
    fn removing_a_pty_leaves_nothing_behind_and_resetting_keeps_the_attachment() {
        let mut runtime = PtyRuntime::new_offline();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        runtime.mark_running_for_test(pty);
        runtime.reset_parser(pty, PtyDimensions::new(4, 16));
        runtime.process_pty_output(pty, b"hello");
        runtime.record_exit_status_for_test(
            pty,
            PtyExit {
                code: 3,
                signal: None,
            },
        );
        runtime.record_foreground_process(
            pty,
            ForegroundProcessInfo {
                root_pid: Some(1),
                foreground_pid: Some(2),
                command: Some("cargo test".to_string()),
            },
        );

        // A reset is one run ending, not the PTY going away: the screen, the
        // "has output" flag and the exit go; the attachment stays.
        runtime.reset_parser(pty, PtyDimensions::new(4, 16));
        assert!(runtime.is_running(pty));
        assert!(runtime.pty_output_is_blank(pty));
        assert!(runtime.pty_exit_status(pty).is_none());
        assert_eq!(runtime.pty_last_command(pty), Some("cargo test"));

        runtime.remove_pty(pty);

        assert!(runtime.panes.is_empty());
        assert!(runtime.pane_index.is_empty());
        assert!(!runtime.is_running(pty));
        assert!(runtime.parser(pty).is_none());
        assert!(runtime.pty_exit_status(pty).is_none());
        assert_eq!(runtime.pty_last_command(pty), None);
        assert!(runtime.pty_output_is_blank(pty));
    }

    #[test]
    fn output_for_an_unmapped_pane_is_dropped_not_synthesised() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);

        // A pane the client never attached: a PTY `stop`/`remove_pty`
        // just dropped, or an id a rogue daemon invented. Materialising a parser
        // for it leaks 5000 lines of scrollback that nothing ever reclaims and
        // can resurrect deleted content.
        sender
            .send(ServerMessage::PtyOutput {
                pane: SessionId(4_242),
                bytes: b"ghost output".to_vec(),
            })
            .expect("send output for an unmapped pane");
        sender
            .send(ServerMessage::PtyScrollback {
                pane: SessionId(4_242),
                bytes: b"ghost scrollback".to_vec(),
            })
            .expect("send scrollback for an unmapped pane");

        let events = runtime.drain_events();

        assert!(events.is_empty(), "unmapped pane produced {events:?}");
        // The only entry is the attached terminal's, and even that has no
        // parser yet: nothing was materialised for the invented pane id.
        assert_eq!(runtime.panes.len(), 1);
        assert!(runtime.parser(pty).is_none());
        assert!(runtime
            .parser(PtyKey::Terminal(TerminalId::new(4_242).unwrap()))
            .is_none());
    }

    #[test]
    fn closing_a_connection_shuts_the_socket_down_so_the_reader_thread_exits() {
        let (client_stream, server_stream) = UnixStream::pair().expect("create socket pair");
        let reader_stream = client_stream.try_clone().expect("clone reader half");
        let socket = client_stream.try_clone().expect("clone shutdown handle");
        let (_sender, receiver) = mpsc::channel();
        let connection = ServerConnection {
            writer: Arc::new(Mutex::new(client_stream)),
            receiver,
            socket,
        };
        // The reader thread parks in `read_message` on the connection's socket,
        // exactly as `connect_inner` leaves it. The peer end stays open, so the
        // only thing that can wake it is our own shutdown.
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = reader_stream;
            let result = read_message::<ServerMessage>(&mut reader);
            let _ = done.send(result.err().map(|error| error.kind()));
        });

        drop(connection);

        let outcome = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("reader thread should exit once the socket is shut down");
        assert_eq!(outcome, Some(io::ErrorKind::UnexpectedEof));
        drop(server_stream);
    }

    #[test]
    fn drain_events_stops_on_its_per_frame_budget_and_resumes_next_frame() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(3).unwrap());
        let pane = SessionId(3);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(24, 80));

        // A pane producing faster than vt100 consumes. Without a budget the
        // drain never returns and the frame never completes.
        let chunk = vec![b'x'; 16 * 1024];
        let chunks = (DRAIN_MAX_BYTES_PER_FRAME / chunk.len()) * 3;
        for _ in 0..chunks {
            sender
                .send(ServerMessage::PtyOutput {
                    pane,
                    bytes: chunk.clone(),
                })
                .expect("queue output");
        }

        let first = runtime.drain_events();
        let drained: usize = first
            .iter()
            .map(|event| match event {
                PtyEvent::Output { byte_count, .. } => *byte_count,
                _ => 0,
            })
            .sum();

        assert!(!first.is_empty());
        assert!(
            drained < chunk.len() * chunks,
            "the whole queue was drained"
        );
        assert!(runtime.has_deferred_work());

        let mut total = drained;
        let mut frames = 1;
        while runtime.has_deferred_work() {
            total += runtime
                .drain_events()
                .iter()
                .map(|event| match event {
                    PtyEvent::Output { byte_count, .. } => *byte_count,
                    _ => 0,
                })
                .sum::<usize>();
            frames += 1;
            assert!(frames < 100, "budgeted drain made no progress");
        }
        assert_eq!(total, chunk.len() * chunks);
    }

    #[test]
    fn adjacent_output_for_one_pane_is_coalesced_into_a_single_feed() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(3).unwrap());
        let pane = SessionId(3);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(2, 16));

        for part in [&b"one"[..], b"-two", b"-three"] {
            sender
                .send(ServerMessage::PtyOutput {
                    pane,
                    bytes: part.to_vec(),
                })
                .expect("queue output");
        }

        let events = runtime.drain_events();

        assert_eq!(
            events,
            vec![PtyEvent::Output {
                pty,
                byte_count: 13,
            }]
        );
        assert_eq!(runtime.pty_lines(pty)[0], "one-two-three");
    }

    #[test]
    fn pane_less_errors_are_not_attributed_to_an_arbitrary_terminal() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);

        sender
            .send(ServerMessage::Error {
                pane: None,
                code: RejectCode::Unspecified,
                message: "connection-wide failure".to_string(),
            })
            .expect("send pane-less error");
        sender
            .send(ServerMessage::Error {
                pane: Some(pane),
                code: RejectCode::PaneOperationFailed,
                message: "pane failure".to_string(),
            })
            .expect("send pane error");
        sender
            .send(ServerMessage::Error {
                pane: Some(SessionId(1_234)),
                code: RejectCode::UnknownSession,
                message: "unknown pane failure".to_string(),
            })
            .expect("send unmapped pane error");

        let events = runtime.drain_events();

        assert_eq!(
            events,
            vec![
                PtyEvent::Error {
                    pty: None,
                    code: RejectCode::Unspecified,
                    message: "connection-wide failure".to_string(),
                },
                PtyEvent::Error {
                    pty: Some(pty),
                    code: RejectCode::PaneOperationFailed,
                    message: "pane failure".to_string(),
                },
                PtyEvent::Error {
                    pty: None,
                    code: RejectCode::UnknownSession,
                    message: "unknown pane failure".to_string(),
                },
            ]
        );
        // Unattributed errors are held for a global surface rather than being
        // written into whichever pane the hash order yielded.
        assert_eq!(
            runtime.take_last_server_error().as_deref(),
            Some("unknown pane failure")
        );
        assert_eq!(runtime.take_last_server_error(), None);
    }

    #[test]
    fn scroll_up_clamps_instead_of_inverting_on_a_huge_row_count() {
        let mut runtime = PtyRuntime::new_offline();
        let pty = PtyKey::Terminal(TerminalId::new(7).unwrap());
        runtime.reset_parser(pty, PtyDimensions::new(2, 8));
        runtime.process_pty_output(pty, b"one\r\ntwo\r\nthree\r\nfour");

        // `rows as i32` on a value with bit 31 set is negative, which used to
        // turn a scroll up into a scroll down.
        assert!(runtime.scroll_up(pty, usize::MAX));

        assert!(runtime.parser(pty).unwrap().screen().scrollback() > 0);
        assert_eq!(
            runtime.pty_lines(pty),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn terminal_query_responses_are_capped_and_sent_as_one_message() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let pty = PtyKey::Terminal(TerminalId::new(9).unwrap());
        let pane = SessionId(9);
        let mut runtime = test_runtime(client_stream, receiver, pty, pane);
        runtime.reset_parser(pty, PtyDimensions::new(4, 16));

        // A pane stuck in a query loop: one reply per query, each its own socket
        // write, used to turn one read into thousands of protocol messages.
        let mut output = Vec::new();
        for _ in 0..200 {
            output.extend_from_slice(b"\x1b[c\x1b[6n");
        }
        sender
            .send(ServerMessage::PtyOutput {
                pane,
                bytes: output,
            })
            .expect("send query storm");

        let _ = runtime.drain_events();

        let message: ClientMessage = read_message(&mut server_stream).expect("read replies");
        let ClientMessage::Input {
            pane: replied_pane,
            bytes,
        } = message
        else {
            panic!("expected a single batched Input message, got {message:?}");
        };
        assert_eq!(replied_pane, pane);
        // At most one cursor report and at most the per-chunk cap in total.
        assert_eq!(bytes.windows(3).filter(|w| *w == b"\x1b[1").count(), 1);
        let device_attributes = PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.len();
        assert!(bytes.len() <= TERMINAL_MAX_RESPONSES_PER_CHUNK * (device_attributes + 8));
        assert!(bytes.starts_with(PRIMARY_DEVICE_ATTRIBUTES_RESPONSE));

        // Nothing else was written for this chunk.
        server_stream
            .set_nonblocking(true)
            .expect("set nonblocking");
        assert!(read_message::<ClientMessage>(&mut server_stream).is_err());
    }

    /// Reference implementation of the doc claim on `feed_parser_with_responder`:
    /// feed one byte per call, sharing the responder and the chunk budget so the
    /// only difference from the batched feed is the slice size.
    fn feed_per_byte(bytes: &[u8], size: PtyDimensions) -> (Vec<String>, (u16, u16), Vec<u8>) {
        let mut parser = Parser::new(size.rows, size.cols, TERMINAL_SCROLLBACK_LINES);
        let mut responder = TerminalResponseDetector::default();
        let mut budget = TerminalResponseBudget::default();
        let mut responses = Vec::new();
        for byte in bytes {
            responses.extend(feed_parser_with_responder(
                &mut parser,
                &mut responder,
                &mut budget,
                std::slice::from_ref(byte),
            ));
        }
        (
            terminal_screen_rows(&parser),
            parser.screen().cursor_position(),
            responses,
        )
    }

    /// The batched feed under test, optionally split into `chunk` sized pieces
    /// so escape sequences straddle chunk boundaries.
    fn feed_batched(
        bytes: &[u8],
        size: PtyDimensions,
        chunk: usize,
    ) -> (Vec<String>, (u16, u16), Vec<u8>) {
        let mut parser = Parser::new(size.rows, size.cols, TERMINAL_SCROLLBACK_LINES);
        let mut responder = TerminalResponseDetector::default();
        let mut budget = TerminalResponseBudget::default();
        let mut responses = Vec::new();
        for piece in bytes.chunks(chunk.max(1)) {
            responses.extend(feed_parser_with_responder(
                &mut parser,
                &mut responder,
                &mut budget,
                piece,
            ));
        }
        (
            terminal_screen_rows(&parser),
            parser.screen().cursor_position(),
            responses,
        )
    }

    /// xorshift64*, inline so the corpus is varied and every failure is
    /// reproducible from its seed (the workspace takes no new dependencies).
    struct TestRng(u64);

    impl TestRng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }
    }

    /// Byte streams built from the pieces that make the two feeds diverge if
    /// anything is wrong: printable runs, whole and half escape sequences, the
    /// queries that produce replies, and raw control bytes.
    fn generated_stream(rng: &mut TestRng) -> Vec<u8> {
        const FRAGMENTS: [&[u8]; 20] = [
            b"a",
            b"hello world",
            b"\r\n",
            b"\t",
            b"\x1b",
            b"\x1b[",
            b"\x1b[0m",
            b"\x1b[31;1m",
            b"\x1b[6n",
            b"\x1b[?6n",
            b"\x1b[5n",
            b"\x1b[c",
            b"\x1b[0c",
            b"\x1bZ",
            b"\x1b(B",
            b"\x1b]0;title\x07",
            b"\x1bP1;2q data \x1b\\",
            b"\x1b[?1049h",
            b"\x1b[2J",
            b"\x07",
        ];
        let mut stream = Vec::new();
        for _ in 0..rng.below(40) + 1 {
            stream.extend_from_slice(FRAGMENTS[rng.below(FRAGMENTS.len())]);
            if rng.below(8) == 0 {
                // A raw byte, including the ones that terminate a sequence.
                stream.push(rng.next_u64() as u8);
            }
        }
        stream
    }

    #[test]
    fn batched_feed_matches_per_byte_feed() {
        let size = PtyDimensions::new(6, 20);
        let mut adversarial: Vec<Vec<u8>> = vec![
            b"plain text with no escapes".to_vec(),
            b"abc\x1b[6ndef".to_vec(),
            b"\x1b[6n\x1b[6n\x1b[6n".to_vec(),
            // Truncated / dangling sequences at the end of a chunk.
            b"abc\x1b".to_vec(),
            b"abc\x1b[".to_vec(),
            b"abc\x1b[12;".to_vec(),
            b"\x1b]0;unterminated title".to_vec(),
            b"\x1bP dcs with no terminator".to_vec(),
            // Oversized CSI: past the bound the responder stops recording and
            // must still resynchronise on the final byte.
            {
                let mut oversized = b"\x1b[".to_vec();
                oversized.extend(std::iter::repeat_n(
                    b'1',
                    TERMINAL_MAX_CSI_SEQUENCE_BYTES + 40,
                ));
                oversized.extend_from_slice(b"n rest");
                oversized
            },
            b"\x1b\x1b[c".to_vec(),
            b"\x1bZtail".to_vec(),
            b"\x1b[?6n\x1b[5n\x1b[c".to_vec(),
            // Enough output to wrap and scroll, with queries in between.
            {
                let mut scrolling = Vec::new();
                for row in 0..30 {
                    scrolling.extend_from_slice(format!("line {row} padded out\r\n").as_bytes());
                    if row % 7 == 0 {
                        scrolling.extend_from_slice(b"\x1b[6n");
                    }
                }
                scrolling
            },
        ];
        for seed in [1_u64, 17, 99, 4_242, 123_456] {
            let mut rng = TestRng::new(seed);
            for _ in 0..40 {
                adversarial.push(generated_stream(&mut rng));
            }
        }

        for stream in adversarial {
            let reference = feed_per_byte(&stream, size);
            for chunk in [1_usize, 2, 3, 5, 17, usize::MAX] {
                let batched = feed_batched(&stream, size, chunk);
                assert_eq!(
                    batched.0, reference.0,
                    "screen differs at chunk {chunk} for {stream:?}"
                );
                assert_eq!(
                    batched.1, reference.1,
                    "cursor differs at chunk {chunk} for {stream:?}"
                );
                assert_eq!(
                    batched.2, reference.2,
                    "responses differ at chunk {chunk} for {stream:?}"
                );
            }
        }
    }
}
