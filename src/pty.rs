use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env,
    ffi::OsStr,
    fs,
    io::{self, Write},
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
    bounded_screen_dimensions, default_socket_path,
    peer::{effective_uid, verify_peer_is_self},
    read_message,
    shell::{default_shell, shell_command_args},
    write_message, AgentSessionMetadata, AgentStatusError, AgentStatusOutcome, AgentStatusQuery,
    AgentStatusRecord, AttachError, AttachOutcome, AttachmentLease, ClientMessage, ClientScopeId,
    CreateError, CreateOutcome, ForegroundProcessInfo, LaunchSpec, LeaseRejectionReason,
    OutputSequence, PaneId, RejectCode, RequestId, ServerInstanceId, ServerMessage, SessionId,
    SessionIdentity as WireSessionIdentity, SessionInfo, StateNamespace as WireStateNamespace,
    StopError, StopOutcome, MAX_PENDING_REQUESTS_PER_CLIENT, MIN_SCREEN_COLS, MIN_SCREEN_ROWS,
    PROTOCOL_VERSION, SOCKET_PATH_ENV,
};
use vt100::{MouseProtocolEncoding, MouseProtocolMode, Parser};

use crate::model::{
    ChatId, PtyKey, SessionIdentity, StateNamespace, TerminalId, RUNTIME_TERMINAL_ID_FLAG,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtySpawn {
    pub terminal: PtyKey,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub size: PtyDimensions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyDimensions {
    pub rows: u16,
    pub cols: u16,
}

/// A pointer button, as a program that enabled mouse reporting understands it.
///
/// The wheel is a set of buttons in every xterm mouse protocol, not a separate
/// event kind, which is why the notches live here alongside the physical
/// buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyMouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

impl PtyMouseButton {
    /// The button field of the xterm report, before motion and modifier bits.
    fn code(self) -> u8 {
        match self {
            Self::Left => MOUSE_BUTTON_LEFT,
            Self::Middle => MOUSE_BUTTON_MIDDLE,
            Self::Right => MOUSE_BUTTON_RIGHT,
            Self::WheelUp => WHEEL_UP_BUTTON,
            Self::WheelDown => WHEEL_DOWN_BUTTON,
            Self::WheelLeft => WHEEL_LEFT_BUTTON,
            Self::WheelRight => WHEEL_RIGHT_BUTTON,
        }
    }

    /// A wheel notch is reported as a press and never as a release or a drag —
    /// there is no such thing as letting go of the wheel.
    fn is_wheel(self) -> bool {
        matches!(
            self,
            Self::WheelUp | Self::WheelDown | Self::WheelLeft | Self::WheelRight
        )
    }
}

/// What the pointer did, in the vocabulary the mouse protocols distinguish.
///
/// `Drag` is motion with a button held (DECSET 1002) and `Move` is motion with
/// none (DECSET 1003); the two are separate because a program can ask for the
/// first without the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyMouseAction {
    Press(PtyMouseButton),
    Release(PtyMouseButton),
    Drag(PtyMouseButton),
    Move,
}

/// One mouse event addressed to the program running in a pane.
///
/// `col`/`row` are 1-based and screen-relative — the coordinate space of the
/// pane's own emulator, not of the host terminal — so the caller is responsible
/// for hit-testing the pane before building one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyMouseReport {
    pub action: PtyMouseAction,
    pub col: u16,
    pub row: u16,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Notifications drained by the render loop. `Scrollback` and `Output` report
/// only *how much* arrived: the bytes themselves are already committed to the
/// terminal's parser by the time the event is queued, so carrying a copy of
/// every chunk through the queue allocated per chunk for nobody to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    Scrollback {
        terminal: PtyKey,
        byte_count: usize,
    },
    ReplayTruncated {
        terminal: PtyKey,
        omitted_bytes: u64,
    },
    Output {
        terminal: PtyKey,
        byte_count: usize,
    },
    TakenOver {
        terminal: PtyKey,
    },
    Exited {
        terminal: PtyKey,
        status: PtyExit,
    },
    Error {
        terminal: PtyKey,
        message: String,
    },
    /// A failure that belongs to the *connection*, not to any one pane: the
    /// daemon could not be reached, its protocol version does not match, the
    /// socket went away, or it sent a connection-wide `ServerMessage::Error`
    /// (the protocol reserves `LeaseRejected` for per-pane failures, so this
    /// carries no pane and must not invent one — B8).
    ///
    /// These used to be attributed to `PtyKey::Terminal(TerminalId(0))`, an id
    /// that cannot exist, so the diagnostic was written into a pane nobody
    /// could open. The render loop routes them to the status surface (E2).
    ConnectionError {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyDeliveryOperation {
    Input,
    Paste,
    Resize,
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyDeliveryError {
    pub operation: PtyDeliveryOperation,
    pub pane: PaneId,
}

impl std::fmt::Display for PtyDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?} delivery to pane {} is uncertain; it was not replayed",
            self.operation, self.pane.0
        )
    }
}

impl std::error::Error for PtyDeliveryError {}

/// Why a [`PtyRuntime`] operation failed.
///
/// `io::Error` used to be this module's universal error type, so callers
/// classified failures by `ErrorKind` — `NotFound` meaning "the daemon has no
/// such session", `NotConnected` meaning "offline" — or by matching substrings
/// of a message. Both are lossy and neither is a contract: `ErrorKind` has no
/// variant for most of what the daemon can say, and rewording a message
/// silently changes behaviour (F8). Every daemon refusal now arrives as a
/// [`RejectCode`], and `io::Result` survives only at the true I/O boundary —
/// sockets, files, and the framing helpers in `mult_protocol`.
///
/// Written by hand rather than derived: this workspace adds no dependencies,
/// so there is no `thiserror`.
#[derive(Debug)]
pub enum PtyError {
    /// The daemon refused the operation. `code` is the contract that callers
    /// switch on; `detail` is the daemon's diagnostic text and is never parsed.
    Rejected { code: RejectCode, detail: String },
    /// There is no daemon connection. A connector may already be running, so
    /// the next attempt can succeed without the caller doing anything.
    Offline(String),
    /// The connection dropped while a request was outstanding.
    Disconnected(String),
    /// A correlated request did not complete within its deadline.
    Timeout(String),
    /// A frame was written and then failed. The daemon may have received none,
    /// part, or all of it, so these are never replayed automatically.
    DeliveryUncertain(PtyDeliveryError),
    /// The daemon spoke out of the order the protocol requires. The attachment
    /// is left unreconciled; only a fresh attach replay can rebuild it.
    ProtocolViolation(String),
    /// The caller asked for something it may not ask for: a terminal with no
    /// registered durable identity, or one that is already attached. A local
    /// bug or a misuse, never a daemon condition.
    Invalid(String),
    /// Socket or OS I/O failed.
    Io(io::Error),
}

pub type PtyResult<T> = Result<T, PtyError>;

impl PtyError {
    /// The daemon's machine-readable reason, when the daemon is the one that
    /// refused. Local failures have none.
    pub fn code(&self) -> Option<RejectCode> {
        match self {
            Self::Rejected { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Whether the request should be re-sent over a fresh connection rather
    /// than reported. Only the transport is at fault here; the daemon never
    /// saw, or never answered, the request.
    fn is_disconnected(&self) -> bool {
        match self {
            Self::Offline(_) | Self::Disconnected(_) => true,
            Self::Io(error) => matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::NotConnected
            ),
            _ => false,
        }
    }

    /// Whether this failure leaves the pane's attachment in an unknown state.
    ///
    /// When it does, the local pane mapping and lease are kept: only a takeover,
    /// a scoped rejection, a correlated stop/exit or a fresh attach may prove
    /// what the daemon actually did. A definite rejection is the opposite — the
    /// daemon answered, so the attachment can be torn down at once.
    pub fn is_reconciliation_uncertain(&self) -> bool {
        match self {
            Self::Offline(_)
            | Self::Disconnected(_)
            | Self::Timeout(_)
            | Self::DeliveryUncertain(_)
            | Self::ProtocolViolation(_) => true,
            Self::Io(error) => matches!(
                error.kind(),
                io::ErrorKind::TimedOut
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::NotConnected
            ),
            Self::Rejected { .. } | Self::Invalid(_) => false,
        }
    }

    /// The uncertain delivery this failure describes, if any. The runtime uses
    /// it to name the operation whose bytes may or may not have landed.
    pub fn delivery(&self) -> Option<&PtyDeliveryError> {
        match self {
            Self::DeliveryUncertain(error) => Some(error),
            _ => None,
        }
    }
}

impl std::fmt::Display for PtyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { code, detail } if detail.is_empty() => write!(formatter, "{code}"),
            Self::Rejected { code, detail } => write!(formatter, "{code}: {detail}"),
            Self::Offline(message)
            | Self::Disconnected(message)
            | Self::Timeout(message)
            | Self::ProtocolViolation(message)
            | Self::Invalid(message) => formatter.write_str(message),
            Self::DeliveryUncertain(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PtyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DeliveryUncertain(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for PtyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PtyDeliveryError> for PtyError {
    fn from(error: PtyDeliveryError) -> Self {
        Self::DeliveryUncertain(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyExit {
    pub code: u32,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachExistingResult {
    Attached,
    Missing,
}

pub struct PtyRuntime {
    socket_path: PathBuf,
    connection: Option<ServerConnection>,
    // Everything the client tracks per PTY key. This was twelve parallel maps
    // (ten keyed by `PtyKey`, two by `PaneId`), so every lifecycle operation
    // had to remember which subset to touch and an omission leaked state
    // silently (F2). One entry per key means `remove_terminal` cannot forget a
    // field and a new field is tracked everywhere by construction.
    panes: HashMap<PtyKey, PtyPane>,
    // Reverse index for the `PaneId`s the daemon addresses its messages to.
    // Holds exactly the keys whose `PtyPane::attachment` is `Some`; it is
    // maintained only by [`PtyRuntime::bind_pane`] and
    // [`PtyRuntime::clear_attachment`], never on its own.
    pane_index: HashMap<PaneId, PtyKey>,
    pending_events: Vec<PtyEvent>,
    client_scope: Option<ClientScopeId>,
    server_instance: Option<ServerInstanceId>,
    next_request_id: Option<RequestId>,
    pending_requests: HashSet<RequestId>,
    deferred_messages: VecDeque<ServerMessage>,
    // Escape sequences addressed to the *user's own* terminal rather than to a
    // pane — today only OSC 52 clipboard writes. They are queued here because
    // every input handler already carries `&mut PtyRuntime`, and the render
    // loop (which owns the frame's writer) drains them immediately after
    // `Terminal::draw`, so they leave through the frame's output path instead
    // of a separate `io::stdout()` handle grabbed mid-handler.
    host_terminal_writes: Vec<u8>,
    // The terminal currently being created by `start`, if any. It is excluded
    // from reconnect re-attach because its session does not exist on the server
    // until `start`'s own CreateSession completes.
    starting: Option<PtyKey>,
    // Terminals whose attachment must be rebuilt after a (re)connection, oldest
    // first. Re-attaching is one synchronous round trip *per terminal*, so it is
    // queued here and serviced a bounded slice at a time by
    // [`Self::service_reattachments`] instead of running all N round trips in
    // whichever frame happened to notice the reconnect.
    pending_reattach: VecDeque<PtyKey>,
    // The in-flight background connection attempt, if any. At most one exists at
    // a time; see [`PendingConnect`].
    pending_connect: Option<PendingConnect>,
    // Earliest instant at which a *background* reconnect or re-attach may be
    // retried. A dead or wedged daemon must not be retried on every frame.
    retry_not_before: Option<Instant>,
    // Current backoff applied on the next failed background attempt.
    retry_backoff: Duration,
    // Whether the current disconnection was already reported to the UI, so a
    // permanently dead daemon produces one system line rather than one per retry.
    disconnect_reported: bool,
    // Set when the last `drain_events` stopped short of an empty queue, or left
    // re-attachments queued, so the render loop knows to come back for the rest.
    work_remaining: bool,
    // The pane this runtime last told it had the keyboard, so the transition —
    // not the state — is what generates a focus report. Held here rather than in
    // `App` because it is a record of what was *sent*, and only this type sends
    // it; a second copy of that answer could disagree with the wire.
    focused_pane: Option<PtyKey>,
}

const SERVER_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const ATTACH_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// Slots in the reader thread's queue. Each slot can hold an 8 KiB PTY chunk,
/// so this bounds the client-side backlog at roughly 2 MiB if the render thread
/// stalls; at 4096 it was ~32 MiB. Overflow simply parks the reader thread,
/// which applies backpressure to the daemon instead of buffering for it, and
/// the synchronous request loops drain the queue themselves while they wait.
const SERVER_EVENT_QUEUE_CAPACITY: usize = 256;
/// Ceiling on server messages consumed by a single [`PtyRuntime::drain_events`].
/// Without one, a pane producing faster than the parser consumes keeps
/// `try_recv` returning `Ok` forever and the frame never reaches the input poll.
/// Anything left over stays queued and is reported by
/// [`PtyRuntime::has_pending_work`], which keeps the render loop coming back.
const MAX_SERVER_MESSAGES_PER_DRAIN: usize = 128;
/// Companion byte ceiling for the same drain: 128 messages of 8 KiB is already a
/// megabyte of parser work, and replay chunks are larger still.
const MAX_SERVER_OUTPUT_BYTES_PER_DRAIN: usize = 256 * 1024;
/// Wall-clock budget for re-attaching queued terminals inside one drain. A
/// single attach can still overrun it — its own `ATTACH_ACK_TIMEOUT` is not
/// preemptible — but the budget stops the loop from *starting* another attach
/// once it is spent, so a frame costs at most one stalled round trip instead of
/// one per terminal. A healthy daemon answers in microseconds and drains the
/// whole queue in the first frame.
const REATTACH_FRAME_BUDGET: Duration = Duration::from_millis(100);
/// Backoff between failed background reconnect/re-attach attempts. The first
/// retry waits `MIN` after the failure and each further failure doubles the wait
/// up to `MAX`, so a daemon that is gone for good costs one attempt every five
/// seconds rather than one per frame.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(250);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
const TERMINAL_SCROLLBACK_LINES: usize = 5_000;
const TERMINAL_MAX_CSI_SEQUENCE_BYTES: usize = 128;
const PRIMARY_DEVICE_ATTRIBUTES_RESPONSE: &[u8] = b"\x1b[?1;2c";
const DEVICE_STATUS_OK_RESPONSE: &[u8] = b"\x1b[0n";
/// Ceiling on terminal-query answers produced from one chunk of PTY output.
/// See [`feed_parser_with_responder`].
const MAX_TERMINAL_QUERY_RESPONSES_PER_CHUNK: usize = 8;
/// Ceiling on the coalesced answer payload for one chunk of PTY output.
const MAX_TERMINAL_QUERY_RESPONSE_BYTES: usize = 256;
/// Ceiling on sequences queued for the host terminal between two frames.
const MAX_HOST_TERMINAL_WRITE_BYTES: usize = 1024 * 1024;
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
/// Ceiling on the characters kept from a program-supplied window title. Far
/// past any sidebar width, so it costs nothing legitimate; see
/// [`sanitize_pane_title`].
const MAX_PANE_TITLE_CHARS: usize = 128;
/// DECSET parameter for focus reporting: the program asks to be told when the
/// terminal window gains (`CSI I`) or loses (`CSI O`) focus.
const FOCUS_REPORTING_MODE: usize = 1004;
const FOCUS_IN_REPORT: &[u8] = b"\x1b[I";
const FOCUS_OUT_REPORT: &[u8] = b"\x1b[O";
/// xterm mouse button bits. The low two bits number the button, bit 5 marks a
/// motion event, and bit 6 marks a wheel notch; the modifier bits are added on
/// top by [`MouseReport::modifier_bits`].
const MOUSE_BUTTON_LEFT: u8 = 0;
const MOUSE_BUTTON_MIDDLE: u8 = 1;
const MOUSE_BUTTON_RIGHT: u8 = 2;
/// "No button": what a bare motion event reports, and what the legacy
/// encodings report on *any* release, since they cannot say which button was
/// let go.
const MOUSE_BUTTON_NONE: u8 = 3;
const MOUSE_MOTION_BIT: u8 = 32;
const WHEEL_UP_BUTTON: u8 = 64;
const WHEEL_DOWN_BUTTON: u8 = 65;
const WHEEL_LEFT_BUTTON: u8 = 66;
const WHEEL_RIGHT_BUTTON: u8 = 67;
const MOUSE_SHIFT_BIT: u8 = 4;
const MOUSE_ALT_BIT: u8 = 8;
const MOUSE_CTRL_BIT: u8 = 16;

struct ServerConnection {
    writer: Arc<Mutex<UnixStream>>,
    receiver: Receiver<ServerMessage>,
}

/// Everything [`PtyRuntime`] tracks for one [`PtyKey`].
///
/// An entry exists as soon as *any* of these is recorded — a durable identity
/// registered before the session is started, a parser fed a system line while
/// offline — and lives until [`PtyRuntime::remove_terminal`] drops it. The
/// fields are deliberately independent `Option`s rather than one "attached"
/// sum type, because they genuinely have different lifetimes: a parser
/// outlives an attachment, `lease`/`expected_output` are cleared on a
/// reconnect that keeps the binding, and an identity outlives both.
#[derive(Default)]
struct PtyPane {
    /// The server pane this key is bound to, mirrored by
    /// [`PtyRuntime::pane_index`].
    attachment: Option<PaneId>,
    /// The attachment's lease, present only once an attach replay has been
    /// reconciled. A binding without a lease is a pane awaiting re-attachment.
    lease: Option<AttachmentLease>,
    /// Next output sequence expected on the attachment; set with the lease.
    expected_output: Option<OutputSequence>,
    /// Durable model identity, immutable once registered.
    identity: Option<WireSessionIdentity>,
    /// Chat-agent generation metadata; only ever set for [`PtyKey::ChatAgent`].
    agent: Option<AgentSessionMetadata>,
    parser: Option<Parser>,
    responder: Option<TerminalResponseDetector>,
    /// Whether this key has ever been fed output, so a screen that is blank
    /// because it was cleared is distinguished from one never written to.
    saw_output: bool,
    exit_status: Option<PtyExit>,
    foreground_process: Option<ForegroundProcessInfo>,
    command_tracker: Option<TerminalCommandTracker>,
}

/// A connection attempt running on its own thread.
///
/// Establishing a connection means a blocking `connect(2)`, possibly an
/// autospawn plus a two-second wait for the new daemon's socket, and then a
/// `Hello` round trip with its own two-second timeout. None of that may happen
/// on the render thread, so it happens here and the render thread only ever
/// *collects* the result (see [`PtyRuntime::poll_connector`]), which is a
/// non-blocking `is_finished` check plus a `join` that is already complete.
struct PendingConnect {
    handle: thread::JoinHandle<PtyResult<EstablishedConnection>>,
    /// Client scope this attempt asked the daemon to resume, captured when the
    /// attempt started so a result cannot be judged against a scope that moved
    /// on in the meantime.
    resume: Option<ClientScopeId>,
}

/// A socket that has completed peer verification and the `Hello` exchange but
/// has not yet been adopted by a [`PtyRuntime`] — installation touches runtime
/// state and therefore stays on the owning thread.
struct EstablishedConnection {
    reader: UnixStream,
    writer: UnixStream,
    hello: ServerHello,
}

/// Who produced the bytes being written to a pane.
///
/// The distinction is not cosmetic. Typing ends a scrollback view and feeds the
/// command tracker; a mouse report, a focus notification or an answer to the
/// program's own `CSI 6n` does neither, because the user did not type it. A
/// program polling the cursor position used to yank the reader back to the
/// bottom of the scrollback for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputOrigin {
    /// The user typed or pasted it at the pane.
    User,
    /// The client generated it on the program's behalf.
    Synthetic,
}

#[derive(Debug)]
struct TerminalResponseDetector {
    state: TerminalResponseState,
    /// Whether the program has enabled focus reporting (DECSET 1004). Tracked
    /// here because `vt100` does not model the mode, and this detector is
    /// already scanning every escape sequence the program emits.
    focus_reporting: bool,
    /// Parameter and intermediate bytes of the CSI sequence being scanned.
    ///
    /// Held here rather than inside [`TerminalResponseState::Csi`] so entering
    /// a CSI costs no allocation: a TUI child redrawing a frame emits thousands
    /// of them, and every one used to start with an empty `Vec`. The sequence
    /// was already capped at [`TERMINAL_MAX_CSI_SEQUENCE_BYTES`], so the bound
    /// is the array's length rather than a second check.
    csi: [u8; TERMINAL_MAX_CSI_SEQUENCE_BYTES],
    csi_len: usize,
}

impl Default for TerminalResponseDetector {
    fn default() -> Self {
        Self {
            state: TerminalResponseState::default(),
            focus_reporting: false,
            csi: [0; TERMINAL_MAX_CSI_SEQUENCE_BYTES],
            csi_len: 0,
        }
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum TerminalResponseState {
    #[default]
    Ground,
    Escape,
    /// Inside a CSI sequence; its bytes accumulate in
    /// [`TerminalResponseDetector::csi`].
    Csi,
    CsiIgnored,
    String {
        esc_seen: bool,
    },
    IgnoreOne,
}

/// Whether a connection attempt may start a daemon that is not already
/// running.
///
/// Autospawning forks a process, so it is never implied: an explicit user
/// action (startup, a keystroke, a `start`/`stop`) asks for it, and background
/// reconnects use [`SpawnPolicy::ConnectOnly`] so a daemon the user shut down
/// is not resurrected behind their back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnPolicy {
    Autospawn,
    ConnectOnly,
}

impl SpawnPolicy {
    fn allows_spawn(self) -> bool {
        matches!(self, Self::Autospawn)
    }
}

impl PtyRuntime {
    /// Construct a runtime bound to `socket_path` and connect to it, reporting
    /// a failed connection as a queued [`PtyEvent::ConnectionError`] rather
    /// than failing construction — the UI must come up and say why it is not
    /// connected.
    ///
    /// There is deliberately no `Default` for this type (F3). Construction
    /// connects a socket and, under [`SpawnPolicy::Autospawn`], forks a
    /// daemon; a `#[derive(Default)]` or `..Default::default()` on any struct
    /// holding a `PtyRuntime` would have done both silently.
    pub fn with_socket_path(socket_path: PathBuf, spawn: SpawnPolicy) -> Self {
        match Self::connect_to_socket_with(socket_path.clone(), spawn) {
            Ok(runtime) => runtime,
            Err(error) => Self::disconnected(
                socket_path,
                vec![PtyEvent::ConnectionError {
                    message: format!("failed to connect to mult-server: {error}"),
                }],
            ),
        }
    }

    /// Construct a runtime that has not connected to anything and will not try
    /// until something asks it to. Used by tests and by the fuzz entry point,
    /// which drive the emulator without a daemon.
    pub fn new_offline() -> Self {
        Self::disconnected(default_socket_path(), Vec::new())
    }

    /// As [`Self::with_socket_path`] with [`SpawnPolicy::Autospawn`], but a
    /// connection failure fails construction instead of being queued.
    pub fn connect_to_socket(socket_path: PathBuf) -> PtyResult<Self> {
        Self::connect_to_socket_with(socket_path, SpawnPolicy::Autospawn)
    }

    fn connect_to_socket_with(socket_path: PathBuf, spawn: SpawnPolicy) -> PtyResult<Self> {
        let mut runtime = Self::disconnected(socket_path, Vec::new());
        runtime.connect_inner(spawn)?;
        Ok(runtime)
    }

    fn disconnected(socket_path: PathBuf, pending_events: Vec<PtyEvent>) -> Self {
        Self {
            socket_path,
            connection: None,
            panes: HashMap::new(),
            pane_index: HashMap::new(),
            pending_events,
            client_scope: None,
            server_instance: None,
            next_request_id: Some(RequestId::MIN),
            pending_requests: HashSet::new(),
            deferred_messages: VecDeque::new(),
            host_terminal_writes: Vec::new(),
            starting: None,
            pending_reattach: VecDeque::new(),
            pending_connect: None,
            retry_not_before: None,
            retry_backoff: RECONNECT_BACKOFF_MIN,
            disconnect_reported: false,
            work_remaining: false,
            focused_pane: None,
        }
    }
}

impl PtySpawn {
    pub fn shell(terminal: PtyKey, cwd: Option<PathBuf>, env: BTreeMap<String, String>) -> Self {
        Self {
            terminal,
            program: default_shell(),
            args: Vec::new(),
            cwd,
            env,
            size: PtyDimensions::default(),
        }
    }

    pub fn command_line(
        terminal: PtyKey,
        command: String,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            terminal,
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
        Self { rows: 24, cols: 80 }
    }
}

impl PtyDimensions {
    /// The size the emulator and the daemon will actually be driven at.
    ///
    /// The fields are `pub`, so a private constructor cannot enforce this;
    /// instead every path into the parser and every size put on the wire goes
    /// through here. [`mult_protocol::bounded_screen_dimensions`] owns the
    /// policy — both the memory ceilings and the [`MIN_SCREEN_ROWS`] /
    /// [`MIN_SCREEN_COLS`] floor the parser panics below (A13) — so the client,
    /// the daemon and the wire agree on one answer.
    #[must_use]
    pub fn clamped(self) -> Self {
        let (rows, cols) = bounded_screen_dimensions(self.rows, self.cols);
        Self { rows, cols }
    }

    /// Whether an area of this size is big enough to hold a screen without
    /// being enlarged by [`Self::clamped`]. The renderer uses this to show a
    /// "too small" pane rather than a screen that does not match its area.
    #[must_use]
    pub fn fits_a_screen(self) -> bool {
        self.rows >= MIN_SCREEN_ROWS && self.cols >= MIN_SCREEN_COLS
    }
}

impl PtyRuntime {
    fn pane(&self, terminal: PtyKey) -> Option<&PtyPane> {
        self.panes.get(&terminal)
    }

    fn pane_mut(&mut self, terminal: PtyKey) -> Option<&mut PtyPane> {
        self.panes.get_mut(&terminal)
    }

    /// The entry for `terminal`, created empty if this is the first thing
    /// recorded about it.
    fn pane_entry(&mut self, terminal: PtyKey) -> &mut PtyPane {
        self.panes.entry(terminal).or_default()
    }

    /// The server pane `terminal` is bound to, if it is bound at all. A
    /// binding without a lease is a pane awaiting re-attachment, so this is
    /// weaker than [`Self::is_running`].
    fn attached_pane(&self, terminal: PtyKey) -> Option<PaneId> {
        self.pane(terminal).and_then(|pane| pane.attachment)
    }

    /// Bind `terminal` to `pane`, keeping [`Self::pane_index`] in step. The
    /// lease and watermark are not set here: they arrive with the attach
    /// replay.
    fn bind_pane(&mut self, terminal: PtyKey, pane: PaneId) {
        self.pane_entry(terminal).attachment = Some(pane);
        self.pane_index.insert(pane, terminal);
    }

    fn lease_of(&self, pane: PaneId) -> Option<AttachmentLease> {
        self.key_for_pane(pane)
            .and_then(|terminal| self.pane(terminal))
            .and_then(|entry| entry.lease)
    }

    pub fn is_running(&self, terminal: PtyKey) -> bool {
        self.pane(terminal).is_some_and(|pane| {
            pane.attachment.is_some() && pane.lease.is_some() && pane.expected_output.is_some()
        })
    }

    /// Bind a durable model identity to its runtime key. Rebinding a key to a
    /// different token is refused, so create/attach/stop always use one
    /// immutable logical identity for the lifetime of this adapter.
    pub fn register_session_identity(
        &mut self,
        terminal: PtyKey,
        identity: SessionIdentity,
    ) -> PtyResult<()> {
        let identity = wire_session_identity(identity);
        if self
            .registered_session_identity(terminal)
            .is_some_and(|current| current != identity)
        {
            return Err(PtyError::Invalid(
                "cannot replace an immutable PTY session identity".to_string(),
            ));
        }
        self.pane_entry(terminal).identity = Some(identity);
        Ok(())
    }

    pub fn registered_session_identity(&self, terminal: PtyKey) -> Option<WireSessionIdentity> {
        self.pane(terminal).and_then(|pane| pane.identity)
    }

    pub fn register_agent_session(
        &mut self,
        terminal: PtyKey,
        metadata: AgentSessionMetadata,
    ) -> PtyResult<()> {
        if !matches!(terminal, PtyKey::ChatAgent(_)) {
            return Err(PtyError::Invalid(
                "agent metadata may only be registered for a chat PTY".to_string(),
            ));
        }
        if self.registered_session_identity(terminal).is_none() {
            return Err(PtyError::Invalid(
                "register the durable session identity before agent metadata".to_string(),
            ));
        }
        if self.is_running(terminal)
            && self
                .pane(terminal)
                .and_then(|pane| pane.agent)
                .is_some_and(|current| current != metadata)
        {
            return Err(PtyError::Invalid(
                "cannot replace agent generation while its PTY is running".to_string(),
            ));
        }
        self.pane_entry(terminal).agent = Some(metadata);
        Ok(())
    }

    pub fn parser(&self, terminal: PtyKey) -> Option<&Parser> {
        self.pane(terminal).and_then(|pane| pane.parser.as_ref())
    }

    pub fn terminal_exit_status(&self, terminal: PtyKey) -> Option<&PtyExit> {
        self.pane(terminal)
            .and_then(|pane| pane.exit_status.as_ref())
    }

    pub fn terminal_last_command(&self, terminal: PtyKey) -> Option<&str> {
        self.pane(terminal)
            .and_then(|pane| pane.command_tracker.as_ref())
            .and_then(TerminalCommandTracker::last_command)
    }

    /// The window title the program in `terminal` set for itself (OSC 0/2),
    /// ready to be shown as a label — or `None` when it never set one, or set
    /// one that is blank.
    ///
    /// This is the program's own statement of what it is doing, and it is the
    /// only such statement `mult` has: everything else about a pane is either
    /// what the user launched it as or what
    /// [`Self::terminal_last_command`] guessed by watching the keystrokes go
    /// past. A shell writes its `cwd` here on every prompt, an editor writes
    /// the file it is on, and Claude Code writes what it is working on.
    ///
    /// The string is program-supplied, so it is sanitized on the way out; see
    /// [`sanitize_pane_title`].
    pub fn terminal_title(&self, terminal: PtyKey) -> Option<String> {
        let title = sanitize_pane_title(self.parser(terminal)?.screen().title());
        (!title.is_empty()).then_some(title)
    }

    /// Seed the command tracker without an attached pane.
    ///
    /// Production reaches it only through [`Self::send_input`], which needs a
    /// daemon; a caller that just wants to know what the tracker *would* say —
    /// the sidebar's label precedence, say — should not have to stand one up.
    #[cfg(test)]
    pub fn record_command_for_test(&mut self, terminal: PtyKey, input: &[u8]) {
        self.pane_entry(terminal)
            .command_tracker
            .get_or_insert_with(Default::default)
            .record_input(input);
    }

    #[cfg(test)]
    pub fn mark_running_for_test(&mut self, terminal: PtyKey) {
        self.bind_pane(terminal, pane_for_key(terminal));
        let entry = self.pane_entry(terminal);
        entry.lease = Some(AttachmentLease::MIN);
        entry.expected_output = Some(OutputSequence::ZERO);
    }

    #[cfg(test)]
    pub fn record_exit_status_for_test(&mut self, terminal: PtyKey, status: PtyExit) {
        self.pane_entry(terminal).exit_status = Some(status);
    }

    /// Create a parser for `terminal` if it has none, then size it. Reachable
    /// only from tests and the fuzz entry point; the production paths all go
    /// through [`Self::reset_parser`] or [`Self::resize_parser`], which do not
    /// preserve an existing screen.
    #[cfg(any(test, feature = "fuzzing"))]
    pub fn ensure_parser(&mut self, terminal: PtyKey, size: PtyDimensions) {
        let size = size.clamped();
        self.pane_entry(terminal)
            .parser
            .get_or_insert_with(|| Parser::new(size.rows, size.cols, TERMINAL_SCROLLBACK_LINES));
        self.resize_parser(terminal, size);
    }

    pub fn reset_parser(&mut self, terminal: PtyKey, size: PtyDimensions) {
        let size = size.clamped();
        let entry = self.pane_entry(terminal);
        entry.parser = Some(Parser::new(size.rows, size.cols, TERMINAL_SCROLLBACK_LINES));
        entry.responder = None;
        entry.saw_output = false;
        entry.exit_status = None;
    }

    pub fn remove_terminal(&mut self, terminal: PtyKey) {
        // One entry, so nothing can be forgotten: the whole `PtyPane` goes,
        // and the only external reference to it is the pane index.
        if let Some(pane) = self
            .panes
            .remove(&terminal)
            .and_then(|pane| pane.attachment)
        {
            self.pane_index.remove(&pane);
        }
    }

    pub fn process_terminal_output(&mut self, terminal: PtyKey, bytes: &[u8]) {
        self.feed_terminal_output(terminal, bytes, false);
    }

    fn feed_terminal_output(&mut self, terminal: PtyKey, bytes: &[u8], respond: bool) {
        if bytes.is_empty() {
            return;
        }

        let response = {
            let entry = self.panes.entry(terminal).or_default();
            let parser = entry
                .parser
                .get_or_insert_with(|| Parser::new(24, 80, TERMINAL_SCROLLBACK_LINES));
            let response = if respond {
                let responder = entry.responder.get_or_insert_with(Default::default);
                feed_parser_with_responder(parser, responder, bytes)
            } else {
                // No terminal queries to answer on this path (scrollback replay,
                // local echo, system lines), so feed the whole slice in one call
                // rather than one parser dispatch per byte — a replay can be
                // megabytes.
                parser.process(bytes);
                Vec::new()
            };
            clamp_parser_scrollback(parser);
            response
        };

        self.pane_entry(terminal).saw_output = true;
        if response.is_empty() {
            return;
        }
        // One write per output chunk, never one per query: this runs on the
        // render thread, where every `Input` message is a blocking socket
        // write.
        if let Some(pane) = self.attached_pane(terminal) {
            let _ = self.send_input_inner(terminal, pane, &response, InputOrigin::Synthetic);
        }
    }

    /// Queue an escape sequence for the terminal `mult` itself is drawn on.
    ///
    /// Bounded, so a pathological producer cannot grow the queue without limit
    /// between two frames; an over-budget write is dropped rather than
    /// truncated, since half an escape sequence is worse than none.
    pub fn queue_host_terminal_write(&mut self, bytes: &[u8]) {
        if self.host_terminal_writes.len() + bytes.len() <= MAX_HOST_TERMINAL_WRITE_BYTES {
            self.host_terminal_writes.extend_from_slice(bytes);
        }
    }

    /// Take everything queued for the host terminal. The render loop calls this
    /// after drawing a frame; see [`PtyRuntime::queue_host_terminal_write`].
    pub fn take_host_terminal_writes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.host_terminal_writes)
    }

    pub fn append_terminal_system_line(&mut self, terminal: PtyKey, message: impl AsRef<str>) {
        let line = format!("[mult] {}\r\n", sanitize_system_line(message.as_ref()));
        self.process_terminal_output(terminal, line.as_bytes());
    }

    pub fn terminal_lines(&self, terminal: PtyKey) -> Vec<String> {
        let Some(parser) = self.parser(terminal) else {
            return Vec::new();
        };
        terminal_screen_rows(parser)
    }

    pub fn terminal_all_lines(&self, terminal: PtyKey) -> Vec<String> {
        self.terminal_lines(terminal)
    }

    pub fn terminal_output_is_blank(&self, terminal: PtyKey) -> bool {
        if self.pane(terminal).is_some_and(|pane| pane.saw_output) {
            return false;
        }
        self.parser(terminal)
            .map(|parser| {
                terminal_screen_rows(parser)
                    .iter()
                    .all(|line| line.is_empty())
            })
            .unwrap_or(true)
    }

    /// Attach to a daemon session that must already exist. Unlike [`Self::start`],
    /// this path never sends `CreateSession` and therefore cannot launch a
    /// persisted command as a side effect of client restoration.
    pub fn attach_existing(
        &mut self,
        terminal: PtyKey,
        size: PtyDimensions,
    ) -> PtyResult<AttachExistingResult> {
        let size = size.clamped();
        if self.is_running(terminal) {
            return Ok(AttachExistingResult::Attached);
        }
        self.ensure_connected()?;
        let identity = self.identity_for_key(terminal)?;
        if self.is_running(terminal) {
            return Ok(AttachExistingResult::Attached);
        }
        self.reset_parser(terminal, size);
        let entry = self.pane_entry(terminal);
        entry.foreground_process = None;
        entry.command_tracker = None;
        let session = session_for_key(terminal);
        let pane = pane_for_key(terminal);
        self.bind_pane(terminal, pane);

        let request_id = self.allocate_request()?;
        let request = ClientMessage::Attach {
            request_id,
            identity,
            session,
            rows: size.rows,
            cols: size.cols,
        };
        let result = self.perform_attach(terminal, session, size, request_id, request);
        self.finish_request(request_id);
        match result {
            Ok(()) => Ok(AttachExistingResult::Attached),
            // "The daemon has no such session" is a specific answer, not a
            // shade of `ErrorKind::NotFound`: restoration reports the pane
            // missing and, crucially, never relaunches a persisted command (C1).
            Err(error) if error.code() == Some(RejectCode::UnknownSession) => {
                self.clear_attachment(pane);
                Ok(AttachExistingResult::Missing)
            }
            Err(error) => {
                if !error.is_reconciliation_uncertain() {
                    self.clear_attachment(pane);
                }
                Err(error)
            }
        }
    }

    pub fn start(&mut self, mut spawn: PtySpawn) -> PtyResult<()> {
        spawn.size = spawn.size.clamped();
        if self.is_running(spawn.terminal) {
            return Err(PtyError::Invalid(
                "terminal already has a server attachment".to_string(),
            ));
        }
        if self.attached_pane(spawn.terminal).is_some() {
            match self.attach_existing(spawn.terminal, spawn.size)? {
                AttachExistingResult::Attached => return Ok(()),
                AttachExistingResult::Missing => {}
            }
        }
        self.starting = Some(spawn.terminal);
        let result = self.start_attached(spawn);
        self.starting = None;
        result
    }

    fn start_attached(&mut self, spawn: PtySpawn) -> PtyResult<()> {
        self.ensure_connected()?;
        self.reset_parser(spawn.terminal, spawn.size);
        let entry = self.pane_entry(spawn.terminal);
        entry.foreground_process = None;
        entry.command_tracker = None;
        let agent = entry.agent;
        let identity = self.identity_for_key(spawn.terminal)?;
        let session = session_for_key(spawn.terminal);
        let pane = pane_for_key(spawn.terminal);
        let launch = launch_spec(&spawn);
        let name = session_name(&spawn, &launch);
        self.bind_pane(spawn.terminal, pane);

        let create_id = self.allocate_request()?;
        let create = ClientMessage::CreateSession {
            request_id: create_id,
            identity,
            requested_id: Some(session),
            agent,
            name,
            cwd: spawn.cwd.clone(),
            env: spawn.env.clone(),
            launch,
            rows: spawn.size.rows,
            cols: spawn.size.cols,
        };
        let create_result = self.perform_create(create_id, create);
        self.finish_request(create_id);
        if let Err(error) = create_result {
            if !error.is_reconciliation_uncertain() {
                self.clear_attachment(pane);
            }
            return Err(error);
        }

        let attach_id = self.allocate_request()?;
        let attach = ClientMessage::Attach {
            request_id: attach_id,
            identity,
            session,
            rows: spawn.size.rows,
            cols: spawn.size.cols,
        };
        let result = self.perform_attach(spawn.terminal, session, spawn.size, attach_id, attach);
        self.finish_request(attach_id);
        if let Err(error) = &result {
            if !error.is_reconciliation_uncertain() {
                self.clear_attachment(pane);
            }
        }
        result
    }

    pub fn stop(&mut self, terminal: PtyKey) -> PtyResult<bool> {
        let Some(pane) = self.attached_pane(terminal) else {
            return Ok(false);
        };
        self.ensure_connected()?;
        let lease = self.lease_for_pane(pane)?;
        let identity = self.identity_for_key(terminal)?;
        let request_id = self.allocate_request()?;
        let request = ClientMessage::Stop {
            request_id,
            identity,
            pane,
            lease,
        };
        let result = self.perform_stop(request_id, request);
        self.finish_request(request_id);
        result?;
        self.clear_attachment(pane);
        self.pane_entry(terminal).exit_status = None;
        Ok(true)
    }

    pub fn list_sessions(&mut self, namespace: StateNamespace) -> PtyResult<Vec<SessionInfo>> {
        self.ensure_connected()?;
        let namespace = wire_state_namespace(namespace);
        self.write(&ClientMessage::ListSessions { namespace })?;
        let deadline = Instant::now() + ATTACH_ACK_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PtyError::Timeout(
                    "timed out waiting for the namespaced session list".to_string(),
                ));
            }
            let Some(connection) = self.connection.as_ref() else {
                return Err(PtyError::Disconnected(
                    "mult-server disconnected while listing sessions".to_string(),
                ));
            };
            match connection.receiver.recv_timeout(remaining) {
                Ok(ServerMessage::Sessions {
                    namespace: received,
                    sessions,
                }) if received == namespace => return Ok(sessions),
                Ok(message) => self.route_during_request(message),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(PtyError::Timeout(
                        "timed out waiting for the namespaced session list".to_string(),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.disconnect();
                    return Err(PtyError::Disconnected(
                        "mult-server disconnected while listing sessions".to_string(),
                    ));
                }
            }
        }
    }

    pub fn update_agent_status(
        &mut self,
        record: AgentStatusRecord,
    ) -> PtyResult<AgentStatusRecord> {
        let request_id = self.allocate_request()?;
        let request = ClientMessage::UpdateAgentStatus { request_id, record };
        let outcome = self.perform_agent_status(request_id, request);
        self.finish_request(request_id);
        match outcome? {
            AgentStatusOutcome::Updated(record) => Ok(record),
            AgentStatusOutcome::Error(error) => Err(agent_status_error(error)),
            AgentStatusOutcome::Current(_) => Err(protocol_order_error(
                "status update received a query response",
            )),
        }
    }

    pub fn get_agent_status(
        &mut self,
        query: AgentStatusQuery,
    ) -> PtyResult<Option<AgentStatusRecord>> {
        let request_id = self.allocate_request()?;
        let request = ClientMessage::GetAgentStatus { request_id, query };
        let outcome = self.perform_agent_status(request_id, request);
        self.finish_request(request_id);
        match outcome? {
            AgentStatusOutcome::Current(record) => Ok(record),
            AgentStatusOutcome::Error(error) => Err(agent_status_error(error)),
            AgentStatusOutcome::Updated(_) => Err(protocol_order_error(
                "status query received an update response",
            )),
        }
    }

    pub fn send_input(&mut self, terminal: PtyKey, input: &[u8]) -> PtyResult<bool> {
        let Some(pane) = self.attached_pane(terminal) else {
            return Ok(false);
        };
        self.send_input_inner(terminal, pane, input, InputOrigin::User)?;
        Ok(true)
    }

    fn send_input_inner(
        &mut self,
        terminal: PtyKey,
        pane: PaneId,
        input: &[u8],
        origin: InputOrigin,
    ) -> PtyResult<()> {
        if !input.is_empty() && origin == InputOrigin::User {
            let alternate_screen = self
                .parser(terminal)
                .is_some_and(|parser| parser.screen().alternate_screen());
            if let Some(parser) = self.parser_mut(terminal) {
                parser.set_scrollback(0);
            }
            if !alternate_screen && self.terminal_accepts_shell_input(terminal) {
                self.pane_entry(terminal)
                    .command_tracker
                    .get_or_insert_with(Default::default)
                    .record_input(input);
            }
        }
        self.ensure_connected()?;
        let lease = self.lease_for_pane(pane)?;
        self.write_non_replayable(
            &ClientMessage::Input {
                pane,
                lease,
                bytes: input.to_vec(),
            },
            PtyDeliveryOperation::Input,
            pane,
        )
    }

    fn terminal_accepts_shell_input(&self, terminal: PtyKey) -> bool {
        let Some(process) = self
            .pane(terminal)
            .and_then(|pane| pane.foreground_process.as_ref())
        else {
            return true;
        };
        match (process.root_pid, process.foreground_pid) {
            (Some(root_pid), Some(foreground_pid)) => root_pid == foreground_pid,
            _ => true,
        }
    }

    pub fn send_paste(&mut self, terminal: PtyKey, text: &str) -> PtyResult<bool> {
        let Some(pane) = self.attached_pane(terminal) else {
            return Ok(false);
        };
        let use_bracketed = self
            .parser(terminal)
            .is_some_and(|parser| parser.screen().bracketed_paste());
        let bytes = terminal_paste_bytes(text, use_bracketed);
        if !bytes.is_empty() {
            let alternate_screen = self
                .parser(terminal)
                .is_some_and(|parser| parser.screen().alternate_screen());
            if let Some(parser) = self.parser_mut(terminal) {
                parser.set_scrollback(0);
            }
            if !alternate_screen && self.terminal_accepts_shell_input(terminal) {
                self.pane_entry(terminal)
                    .command_tracker
                    .get_or_insert_with(Default::default)
                    .record_input(&bytes);
            }
        }
        self.ensure_connected()?;
        let lease = self.lease_for_pane(pane)?;
        self.write_non_replayable(
            &ClientMessage::Paste { pane, lease, bytes },
            PtyDeliveryOperation::Paste,
            pane,
        )?;
        Ok(true)
    }

    /// Whether the program in `terminal` has switched on xterm mouse
    /// reporting. When it has, the pointer belongs to the program rather than
    /// to our own selection and scrollback — which for an alternate-screen app
    /// like Claude Code holds nothing to scroll anyway.
    pub fn terminal_reports_mouse(&self, terminal: PtyKey) -> bool {
        self.parser(terminal)
            .is_some_and(|parser| parser.screen().mouse_protocol_mode() != MouseProtocolMode::None)
    }

    /// Whether the program in `terminal` asked to hear about this *kind* of
    /// event, without an event in hand to send.
    ///
    /// [`Self::forward_mouse`] answers the same question, but only by trying:
    /// it either writes the event or silently drops it. The router needs the
    /// answer up front, because "the program does not want this" is not the
    /// same as "nothing happens". A program that never enabled motion tracking
    /// cannot interpret a drag at all, so a drag over its pane is our own text
    /// selection rather than an event dropped on its behalf.
    pub fn terminal_wants_mouse_action(&self, terminal: PtyKey, action: PtyMouseAction) -> bool {
        self.parser(terminal)
            .is_some_and(|parser| action.is_wanted_in(parser.screen().mouse_protocol_mode()))
    }

    /// Forward one mouse event to a mouse-reporting program, encoded in the
    /// protocol *and* the encoding it asked for.
    ///
    /// Returns false — sending nothing — when the terminal has no live
    /// parser/pane, when it is not reporting the mouse, or when the program
    /// asked for a narrower mode than this event belongs to: a program in
    /// press-only X10 mode must not be told about releases, and one that never
    /// enabled motion tracking must not be flooded with pointer movement.
    pub fn forward_mouse(&mut self, terminal: PtyKey, report: PtyMouseReport) -> bool {
        let Some(parser) = self.parser(terminal) else {
            return false;
        };
        let screen = parser.screen();
        let Some(bytes) = mouse_report_bytes(
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            report,
        ) else {
            return false;
        };
        let Some(pane) = self.attached_pane(terminal) else {
            return false;
        };
        match self.send_input_inner(terminal, pane, &bytes, InputOrigin::Synthetic) {
            Ok(()) => true,
            Err(error) => {
                self.pending_events.push(PtyEvent::Error {
                    terminal,
                    message: error.to_string(),
                });
                false
            }
        }
    }

    /// Whether the program in `terminal` asked to be told when it gains and
    /// loses focus (DECSET 1004).
    ///
    /// `vt100` does not track this mode, so it is observed by the same detector
    /// that recognises terminal queries — see
    /// [`TerminalResponseDetector::observe_mode_change`].
    pub fn terminal_reports_focus(&self, terminal: PtyKey) -> bool {
        self.pane(terminal)
            .and_then(|pane| pane.responder.as_ref())
            .is_some_and(|responder| responder.focus_reporting)
    }

    /// Move the keyboard to `terminal`, telling the panes on either side of the
    /// move about it.
    ///
    /// This is the only caller that decides *when* a focus report goes out, and
    /// it does so on the transition: the render loop can hand it the same
    /// answer every tick and nothing is written. A pane that never enabled
    /// DECSET 1004 is silently skipped by [`Self::send_focus`], so this costs a
    /// hash lookup per tick on an ordinary session.
    pub fn set_focused_pane(&mut self, terminal: Option<PtyKey>) {
        if self.focused_pane == terminal {
            return;
        }
        if let Some(previous) = self.focused_pane.take() {
            self.send_focus(previous, false);
        }
        self.focused_pane = terminal;
        if let Some(terminal) = terminal {
            self.send_focus(terminal, true);
        }
    }

    /// Tell a focus-reporting program that it just gained or lost the keyboard.
    ///
    /// In a multiplexer the focused pane is the selected one, so this fires on
    /// a selection change as well as when the host terminal window itself
    /// gains or loses focus. Returns false when the program did not ask.
    pub fn send_focus(&mut self, terminal: PtyKey, gained: bool) -> bool {
        if !self.terminal_reports_focus(terminal) {
            return false;
        }
        let Some(pane) = self.attached_pane(terminal) else {
            return false;
        };
        let bytes = if gained {
            FOCUS_IN_REPORT
        } else {
            FOCUS_OUT_REPORT
        };
        match self.send_input_inner(terminal, pane, bytes, InputOrigin::Synthetic) {
            Ok(()) => true,
            Err(error) => {
                self.pending_events.push(PtyEvent::Error {
                    terminal,
                    message: error.to_string(),
                });
                false
            }
        }
    }

    pub fn scroll_up(&mut self, terminal: PtyKey, rows: usize) -> PtyResult<bool> {
        Ok(self.scroll_parser(terminal, rows as i32))
    }

    pub fn scroll_down(&mut self, terminal: PtyKey, rows: usize) -> PtyResult<bool> {
        Ok(self.scroll_parser(terminal, -(rows.min(i32::MAX as usize) as i32)))
    }

    pub fn resize(&mut self, terminal: PtyKey, size: PtyDimensions) -> PtyResult<()> {
        // Clamped once here so the parser and the daemon's PTY are driven at
        // the same size; the daemon applies the same policy independently.
        let size = size.clamped();
        self.resize_parser(terminal, size);
        let Some(pane) = self.attached_pane(terminal) else {
            return Ok(());
        };
        self.ensure_connected()?;
        let lease = self.lease_for_pane(pane)?;
        let result = self.write_non_replayable(
            &ClientMessage::Resize {
                pane,
                lease,
                rows: size.rows,
                cols: size.cols,
            },
            PtyDeliveryOperation::Resize,
            pane,
        );
        if let Err(error) = &result {
            self.pending_events.push(PtyEvent::Error {
                terminal,
                message: error.to_string(),
            });
        }
        result
    }

    /// Drain one frame's worth of daemon traffic.
    ///
    /// This is the render thread's only scheduled entry point into the runtime,
    /// so everything it does is bounded: at most
    /// `MAX_SERVER_MESSAGES_PER_DRAIN` messages carrying at most
    /// `MAX_SERVER_OUTPUT_BYTES_PER_DRAIN` bytes are parsed, and at most
    /// `REATTACH_FRAME_BUDGET` is spent starting queued re-attachments. Whatever
    /// is left stays queued and is announced by [`Self::has_pending_work`].
    pub fn drain_events(&mut self) -> Vec<PtyEvent> {
        let mut events = std::mem::take(&mut self.pending_events);
        self.work_remaining = false;
        self.poll_connector();
        let was_connected = self.connection.is_some();
        let mut messages = 0usize;
        let mut output_bytes = 0usize;
        while let Some(connection) = self.connection.as_ref() {
            if messages >= MAX_SERVER_MESSAGES_PER_DRAIN
                || output_bytes >= MAX_SERVER_OUTPUT_BYTES_PER_DRAIN
            {
                self.work_remaining = true;
                break;
            }
            match connection.receiver.try_recv() {
                Ok(message) => {
                    messages += 1;
                    output_bytes += server_message_output_bytes(&message);
                    self.handle_server_message(message, &mut events);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnect();
                    break;
                }
            }
        }
        if was_connected && self.connection.is_none() {
            self.reconnect_or_report();
        }
        self.service_reattachments();
        self.retry_connection_if_due();
        events.append(&mut self.pending_events);
        events
    }

    /// Whether the last [`Self::drain_events`] left daemon traffic or queued
    /// re-attachments behind. The render loop uses this to keep requesting a
    /// redraw while work remains, so a per-frame budget throttles the client
    /// rather than stalling it.
    pub fn has_pending_work(&self) -> bool {
        self.work_remaining || !self.pending_reattach.is_empty()
    }

    fn allocate_request(&mut self) -> PtyResult<RequestId> {
        if self.pending_requests.len() >= MAX_PENDING_REQUESTS_PER_CLIENT {
            return Err(PtyError::Invalid(
                "too many pending PTY requests".to_string(),
            ));
        }
        let request_id = self
            .next_request_id
            .ok_or_else(|| io::Error::other("PTY request ID space exhausted"))?;
        self.next_request_id = request_id.checked_next();
        self.pending_requests.insert(request_id);
        Ok(request_id)
    }

    fn finish_request(&mut self, request_id: RequestId) {
        self.pending_requests.remove(&request_id);
        self.deferred_messages
            .retain(|message| message_request_id(message) != Some(request_id));
    }

    fn perform_create(&mut self, request_id: RequestId, request: ClientMessage) -> PtyResult<()> {
        let (expected_identity, expected_session) = match &request {
            ClientMessage::CreateSession {
                identity,
                requested_id,
                ..
            } => (*identity, *requested_id),
            _ => unreachable!("perform_create requires CreateSession"),
        };
        self.write_idempotent_request(&request)?;
        let deadline = Instant::now() + ATTACH_ACK_TIMEOUT;
        loop {
            let message = match self.receive_for_request(request_id, deadline) {
                Ok(message) => message,
                Err(error) if error.is_disconnected() => {
                    self.resume_and_resend(&request)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            match message {
                ServerMessage::CreateResult {
                    request_id: received,
                    outcome,
                } if received == request_id => {
                    return match outcome {
                        CreateOutcome::Created { session }
                            if session.identity == expected_identity
                                && expected_session.is_none_or(|id| id == session.id) =>
                        {
                            Ok(())
                        }
                        CreateOutcome::Created { .. } => Err(protocol_order_error(
                            "create result identifies the wrong logical session",
                        )),
                        CreateOutcome::Error(error) => Err(create_error(error)),
                    };
                }
                ServerMessage::Error { code, message } => {
                    return Err(PtyError::Rejected {
                        code,
                        detail: message,
                    })
                }
                message => self.route_during_request(message),
            }
        }
    }

    fn perform_attach(
        &mut self,
        terminal: PtyKey,
        session: SessionId,
        size: PtyDimensions,
        request_id: RequestId,
        request: ClientMessage,
    ) -> PtyResult<()> {
        // This attach is authoritative for `terminal`, so a queued background
        // re-attachment for it would only duplicate the round trip.
        self.pending_reattach.retain(|queued| *queued != terminal);
        self.write_idempotent_request(&request)?;
        let pane_id = pane_for_key(terminal);
        let deadline = Instant::now() + ATTACH_ACK_TIMEOUT;
        let mut accepted_lease = None;
        let mut replay_next = None;
        let mut replay_watermark = None;
        let mut replay_bytes = Vec::new();
        let mut replay_omitted = 0;
        loop {
            let message = match self.receive_for_request(request_id, deadline) {
                Ok(message) => message,
                Err(error) if error.is_disconnected() => {
                    self.resume_and_resend(&request)?;
                    self.reset_parser(terminal, size);
                    let entry = self.pane_entry(terminal);
                    entry.lease = None;
                    entry.expected_output = None;
                    accepted_lease = None;
                    replay_next = None;
                    replay_watermark = None;
                    replay_bytes.clear();
                    replay_omitted = 0;
                    continue;
                }
                Err(error) => return Err(error),
            };
            match message {
                ServerMessage::AttachResult {
                    request_id: received,
                    outcome,
                } if received == request_id => match outcome {
                    AttachOutcome::Attached {
                        session: attached,
                        pane,
                        lease,
                    } if attached == session && pane.id == pane_id => {
                        if accepted_lease.replace(lease).is_some() {
                            return Err(protocol_order_error("duplicate attach acceptance"));
                        }
                        self.resize_parser(
                            terminal,
                            PtyDimensions {
                                rows: pane.rows,
                                cols: pane.cols,
                            },
                        );
                    }
                    AttachOutcome::Attached { .. } => {
                        return Err(protocol_order_error(
                            "attach result identifies the wrong pane",
                        ));
                    }
                    AttachOutcome::Error(error) => return Err(attach_error(error)),
                },
                ServerMessage::ReplayBegin {
                    request_id: received,
                    pane,
                    lease,
                    first_sequence,
                    watermark,
                    omitted_prefix_bytes,
                } if received == request_id => {
                    if pane != pane_id || accepted_lease != Some(lease) || replay_next.is_some() {
                        return Err(protocol_order_error(
                            "replay began before matching attach acceptance",
                        ));
                    }
                    if first_sequence > watermark || omitted_prefix_bytes != first_sequence.get() {
                        return Err(protocol_order_error(
                            "invalid replay watermark or truncation boundary",
                        ));
                    }
                    replay_next = Some(first_sequence);
                    replay_watermark = Some(watermark);
                    replay_omitted = omitted_prefix_bytes;
                }
                ServerMessage::ReplayChunk {
                    request_id: received,
                    pane,
                    lease,
                    sequence,
                    bytes,
                } if received == request_id => {
                    let Some(expected) = replay_next else {
                        return Err(protocol_order_error(
                            "replay chunk arrived before replay begin",
                        ));
                    };
                    if pane != pane_id || accepted_lease != Some(lease) || sequence != expected {
                        return Err(protocol_order_error(
                            "replay output has a gap, duplicate, or wrong lease",
                        ));
                    }
                    let next = sequence
                        .checked_add_bytes(bytes.len())
                        .ok_or_else(|| protocol_order_error("replay sequence overflow"))?;
                    if replay_watermark.is_some_and(|watermark| next > watermark) {
                        return Err(protocol_order_error("replay chunk exceeds its watermark"));
                    }
                    replay_bytes.extend_from_slice(&bytes);
                    replay_next = Some(next);
                }
                ServerMessage::ReplayEnd {
                    request_id: received,
                    pane,
                    lease,
                    watermark,
                } if received == request_id => {
                    if pane != pane_id
                        || accepted_lease != Some(lease)
                        || replay_watermark != Some(watermark)
                        || replay_next != Some(watermark)
                    {
                        return Err(protocol_order_error(
                            "replay ended with a gap or mismatched watermark",
                        ));
                    }
                    // Commit the replay to the parser only after the complete
                    // transaction validates. A malformed or partial replay can
                    // therefore never leave half-applied terminal state.
                    self.feed_terminal_output(terminal, &replay_bytes, false);
                    self.pending_events.push(PtyEvent::Scrollback {
                        terminal,
                        byte_count: replay_bytes.len(),
                    });
                    if replay_omitted > 0 {
                        self.pending_events.push(PtyEvent::ReplayTruncated {
                            terminal,
                            omitted_bytes: replay_omitted,
                        });
                    }
                    let entry = self.pane_entry(terminal);
                    entry.lease = Some(lease);
                    entry.expected_output = Some(watermark);
                    // A reconnect inside this request (see `resume_and_resend`)
                    // re-queues every tracked terminal, including this one.
                    self.pending_reattach.retain(|queued| *queued != terminal);
                    return Ok(());
                }
                ServerMessage::PtyOutput { pane, lease, .. }
                    if pane == pane_id && accepted_lease == Some(lease) =>
                {
                    return Err(protocol_order_error(
                        "live output arrived before replay end",
                    ));
                }
                ServerMessage::Error { code, message } => {
                    return Err(PtyError::Rejected {
                        code,
                        detail: message,
                    })
                }
                message => self.route_during_request(message),
            }
        }
    }

    fn perform_agent_status(
        &mut self,
        request_id: RequestId,
        request: ClientMessage,
    ) -> PtyResult<AgentStatusOutcome> {
        self.write_idempotent_request(&request)?;
        let deadline = Instant::now() + ATTACH_ACK_TIMEOUT;
        loop {
            let message = match self.receive_for_request(request_id, deadline) {
                Ok(message) => message,
                Err(error) if error.is_disconnected() => {
                    self.resume_and_resend(&request)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            match message {
                ServerMessage::AgentStatusResult {
                    request_id: received,
                    outcome,
                } if received == request_id => return Ok(outcome),
                ServerMessage::Error { code, message } => {
                    return Err(PtyError::Rejected {
                        code,
                        detail: message,
                    })
                }
                message => self.route_during_request(message),
            }
        }
    }

    fn perform_stop(&mut self, request_id: RequestId, request: ClientMessage) -> PtyResult<()> {
        let (stop_pane, stop_lease) = match &request {
            ClientMessage::Stop { pane, lease, .. } => (*pane, *lease),
            _ => unreachable!("perform_stop requires Stop"),
        };
        self.write_idempotent_request(&request)?;
        let deadline = Instant::now() + STOP_ACK_TIMEOUT;
        loop {
            let message = match self.receive_for_request(request_id, deadline) {
                Ok(message) => message,
                Err(error) if error.is_disconnected() => {
                    self.resume_and_resend(&request)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            match message {
                ServerMessage::StopResult {
                    request_id: received,
                    outcome,
                } if received == request_id => {
                    return match outcome {
                        StopOutcome::Stopped { .. } | StopOutcome::AlreadyAbsent => Ok(()),
                        StopOutcome::Error(error @ StopError::LeaseRejected(_)) => {
                            self.clear_attachment(stop_pane);
                            Err(stop_error(error))
                        }
                        StopOutcome::Error(error) => Err(stop_error(error)),
                    };
                }
                ServerMessage::PaneExited { pane, lease, .. }
                    if pane == stop_pane && lease == stop_lease =>
                {
                    // The correlated StopResult is the synchronous completion.
                    // Consume its definitive lifecycle event here so it cannot
                    // be mistaken for a later incarnation reusing this pane ID.
                    self.clear_attachment(pane);
                }
                ServerMessage::Error { code, message } => {
                    return Err(PtyError::Rejected {
                        code,
                        detail: message,
                    })
                }
                message => self.route_during_request(message),
            }
        }
    }

    fn receive_for_request(
        &mut self,
        request_id: RequestId,
        deadline: Instant,
    ) -> PtyResult<ServerMessage> {
        if let Some(index) = self
            .deferred_messages
            .iter()
            .position(|message| message_request_id(message) == Some(request_id))
        {
            return Ok(self
                .deferred_messages
                .remove(index)
                .expect("deferred message index exists"));
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PtyError::Timeout(
                    "timed out waiting for correlated mult-server response".to_string(),
                ));
            }
            let Some(connection) = self.connection.as_ref() else {
                return Err(PtyError::Offline(
                    "not connected to mult-server".to_string(),
                ));
            };
            match connection.receiver.recv_timeout(remaining) {
                Ok(message) if message_request_id(&message).is_some_and(|id| id != request_id) => {
                    self.deferred_messages.push_back(message);
                }
                Ok(message) => return Ok(message),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(PtyError::Timeout(
                        "timed out waiting for correlated mult-server response".to_string(),
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.disconnect();
                    return Err(PtyError::Disconnected(
                        "mult-server disconnected before correlated response".to_string(),
                    ));
                }
            }
        }
    }

    fn route_during_request(&mut self, message: ServerMessage) {
        let mut events = Vec::new();
        self.handle_server_message(message, &mut events);
        self.pending_events.extend(events);
    }

    fn parser_mut(&mut self, terminal: PtyKey) -> Option<&mut Parser> {
        self.pane_mut(terminal)
            .and_then(|pane| pane.parser.as_mut())
    }

    fn resize_parser(&mut self, terminal: PtyKey, size: PtyDimensions) {
        let size = size.clamped();
        let parser = self
            .pane_entry(terminal)
            .parser
            .get_or_insert_with(|| Parser::new(24, 80, TERMINAL_SCROLLBACK_LINES));
        repair_wide_cells_before_narrowing(parser, size);
        parser.set_size(size.rows, size.cols);
        clamp_parser_scrollback(parser);
    }

    fn scroll_parser(&mut self, terminal: PtyKey, rows: i32) -> bool {
        if rows == 0 {
            return false;
        }
        let Some(parser) = self.parser_mut(terminal) else {
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
            ServerMessage::Hello { .. } | ServerMessage::Sessions { .. } => {}
            message @ (ServerMessage::CreateResult { .. }
            | ServerMessage::AttachResult { .. }
            | ServerMessage::ReplayBegin { .. }
            | ServerMessage::ReplayChunk { .. }
            | ServerMessage::ReplayEnd { .. }
            | ServerMessage::StopResult { .. }
            | ServerMessage::AgentStatusResult { .. }) => {
                self.deferred_messages.push_back(message);
            }
            ServerMessage::ForegroundProcess {
                pane,
                lease,
                process,
            } => {
                if self.lease_of(pane) == Some(lease) {
                    if let Some(terminal) = self.key_for_pane(pane) {
                        self.record_foreground_process(terminal, process);
                    }
                }
            }
            ServerMessage::PtyOutput {
                pane,
                lease,
                sequence,
                bytes,
            } => {
                if self.lease_of(pane) != Some(lease) {
                    return;
                }
                let Some(expected) = self
                    .key_for_pane(pane)
                    .and_then(|terminal| self.pane(terminal))
                    .and_then(|entry| entry.expected_output)
                else {
                    if let Some(terminal) = self.key_for_pane(pane) {
                        events.push(PtyEvent::Error {
                            terminal,
                            message: "live PTY output arrived before replay completion".to_string(),
                        });
                    }
                    return;
                };
                if sequence != expected {
                    self.mark_attachment_unreconciled(
                        pane,
                        events,
                        format!(
                            "PTY output sequence gap: expected {}, received {}; a fresh attach replay is required",
                            expected.get(),
                            sequence.get()
                        ),
                    );
                    return;
                }
                let Some(next) = sequence.checked_add_bytes(bytes.len()) else {
                    self.mark_attachment_unreconciled(
                        pane,
                        events,
                        "PTY output sequence overflow; a fresh attach replay is required"
                            .to_string(),
                    );
                    return;
                };
                if let Some(terminal) = self.key_for_pane(pane) {
                    self.pane_entry(terminal).expected_output = Some(next);
                    self.feed_terminal_output(terminal, &bytes, true);
                    push_output_event(events, terminal, bytes.len());
                }
            }
            ServerMessage::PaneExited { pane, lease, exit } => {
                if self.lease_of(pane) != Some(lease) {
                    return;
                }
                if let Some(terminal) = self.clear_attachment(pane) {
                    let status = PtyExit {
                        code: exit.code,
                        signal: exit.signal,
                    };
                    self.pane_entry(terminal).exit_status = Some(status.clone());
                    events.push(PtyEvent::Exited { terminal, status });
                }
            }
            ServerMessage::TakenOver { pane, lease } => {
                if self.lease_of(pane) == Some(lease) {
                    if let Some(terminal) = self.clear_attachment(pane) {
                        events.push(PtyEvent::TakenOver { terminal });
                    }
                }
            }
            ServerMessage::LeaseRejected {
                pane,
                lease,
                operation,
                reason,
            } => {
                if self.lease_of(pane) == Some(lease) {
                    // A refused write is the one rejection that does not mean
                    // the lease is gone: the pane is still ours and its writer
                    // queue will drain. Tearing the attachment down here would
                    // trade a wedged child for a full re-attach and replay. The
                    // daemon could not say so before protocol 11, because it had
                    // to reuse `NotOwner` for a full queue (F8).
                    let terminal = if reason == LeaseRejectionReason::WriteRefused {
                        self.key_for_pane(pane)
                            .unwrap_or_else(|| key_for_pane_id(pane))
                    } else {
                        self.clear_attachment(pane)
                            .unwrap_or_else(|| key_for_pane_id(pane))
                    };
                    events.push(PtyEvent::Error {
                        terminal,
                        message: format!(
                            "{:?} rejected for pane {}: {}",
                            operation,
                            pane.0,
                            reason.code()
                        ),
                    });
                }
            }
            // Connection-wide by protocol definition; a per-pane failure
            // arrives as `LeaseRejected` above. Picking an arbitrary attached
            // pane (or `Terminal(0)`) to blame was wrong in both directions —
            // it hid the error when nothing was attached and slandered an
            // unrelated pane when something was (B8).
            ServerMessage::Error { code, message } => {
                events.push(PtyEvent::ConnectionError {
                    message: format!("{code}: {message}"),
                });
            }
        }
    }

    fn mark_attachment_unreconciled(
        &mut self,
        pane: PaneId,
        events: &mut Vec<PtyEvent>,
        message: String,
    ) {
        if let Some(terminal) = self.key_for_pane(pane) {
            let entry = self.pane_entry(terminal);
            entry.lease = None;
            entry.expected_output = None;
            events.push(PtyEvent::Error { terminal, message });
        }
    }

    /// Drop the binding between a key and a server pane, along with the lease
    /// and watermark that only make sense while it exists. Everything else the
    /// key owns — parser, identity, exit status, trackers — survives, because
    /// the model may still show it and a later attach may reuse it.
    fn clear_attachment(&mut self, pane: PaneId) -> Option<PtyKey> {
        let terminal = self.pane_index.remove(&pane)?;
        if let Some(entry) = self.pane_mut(terminal) {
            entry.attachment = None;
            entry.lease = None;
            entry.expected_output = None;
        }
        Some(terminal)
    }

    fn identity_for_key(&self, terminal: PtyKey) -> PtyResult<WireSessionIdentity> {
        if let Some(identity) = self.registered_session_identity(terminal) {
            return Ok(identity);
        }
        #[cfg(test)]
        {
            Ok(test_wire_session_identity(terminal))
        }
        #[cfg(not(test))]
        {
            Err(PtyError::Invalid(format!(
                "PTY session {terminal:?} has no registered durable identity"
            )))
        }
    }

    fn lease_for_pane(&self, pane: PaneId) -> PtyResult<AttachmentLease> {
        self.lease_of(pane).ok_or_else(|| {
            PtyError::Disconnected(format!(
                "pane {} attachment has not been reconciled",
                pane.0
            ))
        })
    }

    fn key_for_pane(&self, pane: PaneId) -> Option<PtyKey> {
        self.pane_index.get(&pane).copied()
    }

    fn record_foreground_process(&mut self, terminal: PtyKey, process: ForegroundProcessInfo) {
        let foreground_is_child = matches!(
            (process.root_pid, process.foreground_pid),
            (Some(root_pid), Some(foreground_pid)) if root_pid != foreground_pid
        );
        if foreground_is_child {
            if let Some(command) = process.command.as_deref() {
                self.pane_entry(terminal)
                    .command_tracker
                    .get_or_insert_with(Default::default)
                    .record_process_command(command);
            }
        }
        self.pane_entry(terminal).foreground_process = Some(process);
    }

    /// Guarantee a live connection for a caller that is about to send.
    ///
    /// This never establishes a connection itself: it collects one the connector
    /// thread already finished, and otherwise starts a connector and reports
    /// `NotConnected` right away. A `start`, `stop`, resize or keystroke issued
    /// while the client is disconnected therefore **fails visibly and
    /// immediately** instead of freezing the frame for up to eight seconds; the
    /// connection lands in the background and the next attempt succeeds.
    fn ensure_connected(&mut self) -> PtyResult<()> {
        self.poll_connector();
        if self.connection.is_none() {
            // An explicit user action is worth an immediate attempt, so this
            // bypasses the retry backoff — but never runs two connectors at once.
            self.start_connector(SpawnPolicy::Autospawn);
            return Err(PtyError::Offline(
                "not connected to mult-server; a connection attempt is in progress".to_string(),
            ));
        }
        self.service_reattachments();
        Ok(())
    }

    /// React to a connection that just went away: hand the reconnect to the
    /// connector thread and report the loss once.
    fn reconnect_or_report(&mut self) {
        // Background reconnects never autospawn a daemon; only an explicit user
        // action may start one.
        self.start_connector(SpawnPolicy::ConnectOnly);
        self.report_disconnect("a background reconnect is in progress".to_string());
    }

    /// Report daemon loss to the UI at most once per disconnection. A daemon
    /// that is gone for good otherwise writes one system line per retry.
    fn report_disconnect(&mut self, reason: String) {
        if self.disconnect_reported {
            return;
        }
        self.disconnect_reported = true;
        self.pending_events.push(PtyEvent::ConnectionError {
            message: format!(
                "mult-server connection lost; attachment state is retained pending reconciliation: {reason}"
            ),
        });
    }

    /// Queue every tracked terminal for re-attachment after a (re)connection.
    ///
    /// The terminal currently inside `start` is excluded: its session does not
    /// exist on the daemon until `start`'s own `CreateSession` completes.
    fn enqueue_reattachments(&mut self) {
        let terminals = self
            .panes
            .iter()
            .filter(|(terminal, pane)| {
                pane.attachment.is_some() && Some(**terminal) != self.starting
            })
            .map(|(terminal, _)| *terminal)
            .collect::<Vec<_>>();
        for terminal in terminals {
            if !self.pending_reattach.contains(&terminal) {
                self.pending_reattach.push_back(terminal);
            }
        }
    }

    /// Re-attach queued terminals, oldest first, for at most one
    /// [`REATTACH_FRAME_BUDGET`].
    ///
    /// Each re-attach is a synchronous correlated round trip with its own
    /// `ATTACH_ACK_TIMEOUT`, which is exactly why the queue exists: a wedged
    /// daemon costs one such round trip per frame instead of N, and the frame in
    /// between still draws and reads input. The queue is never serviced from
    /// inside another correlated request — `pending_requests` being non-empty
    /// means a `perform_*` loop owns the receiver, and re-entering it here would
    /// interleave two request state machines.
    fn service_reattachments(&mut self) {
        if self.connection.is_none() || !self.pending_requests.is_empty() {
            return;
        }
        if self
            .retry_not_before
            .is_some_and(|not_before| Instant::now() < not_before)
        {
            return;
        }

        let started = Instant::now();
        while self.connection.is_some() {
            let Some(terminal) = self.pending_reattach.pop_front() else {
                break;
            };
            // The queue is advisory: a terminal that was retired or that `start`
            // has since taken over must not be resurrected by a stale entry.
            if self.attached_pane(terminal).is_none() || Some(terminal) == self.starting {
                continue;
            }
            if !self.reattach_terminal(terminal) {
                // Keep the terminal queued and stop starting new round trips
                // until the backoff expires, so a wedged daemon is retried on a
                // schedule instead of once per frame.
                self.pending_reattach.push_back(terminal);
                self.advance_retry_backoff();
                break;
            }
            self.reset_retry_backoff();
            if started.elapsed() >= REATTACH_FRAME_BUDGET {
                break;
            }
        }
    }

    /// Re-attach one terminal. Returns false when the attempt failed in a way
    /// that should be retried later; a vanished session is *not* such a failure
    /// and retires the terminal instead.
    fn reattach_terminal(&mut self, terminal: PtyKey) -> bool {
        let size = self.parser_dimensions(terminal);
        self.reset_parser(terminal, size);
        let Ok(identity) = self.identity_for_key(terminal) else {
            // Without a durable identity the attach can never succeed; drop it
            // from the queue rather than retrying it every backoff round.
            return true;
        };
        let session = session_for_key(terminal);
        let Ok(request_id) = self.allocate_request() else {
            return false;
        };
        let request = ClientMessage::Attach {
            request_id,
            identity,
            session,
            rows: size.rows,
            cols: size.cols,
        };
        let result = self.perform_attach(terminal, session, size, request_id, request);
        self.finish_request(request_id);
        match result {
            Ok(()) => true,
            // The daemon says the session is gone: report the pane exited
            // rather than treating it as a transport problem.
            Err(error) if error.code() == Some(RejectCode::UnknownSession) => {
                let pane = pane_for_key(terminal);
                self.clear_attachment(pane);
                let status = PtyExit {
                    code: 1,
                    signal: Some("server session unavailable".to_string()),
                };
                self.pane_entry(terminal).exit_status = Some(status.clone());
                self.pending_events
                    .push(PtyEvent::Exited { terminal, status });
                true
            }
            Err(error) => {
                self.report_disconnect(error.to_string());
                false
            }
        }
    }

    /// Start a background reconnect once the backoff has expired. Only tracked
    /// attachments justify reconnecting on their own: with nothing to reconcile,
    /// the next user action starts a connector instead.
    fn retry_connection_if_due(&mut self) {
        if self.connection.is_some()
            || self.pending_connect.is_some()
            || !self.panes.values().any(|pane| pane.attachment.is_some())
        {
            return;
        }
        if self
            .retry_not_before
            .is_some_and(|not_before| Instant::now() < not_before)
        {
            return;
        }
        self.start_connector(SpawnPolicy::ConnectOnly);
    }

    fn reset_retry_backoff(&mut self) {
        self.retry_not_before = None;
        self.retry_backoff = RECONNECT_BACKOFF_MIN;
    }

    fn advance_retry_backoff(&mut self) {
        self.retry_not_before = Some(Instant::now() + self.retry_backoff);
        self.retry_backoff = (self.retry_backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }

    fn parser_dimensions(&self, terminal: PtyKey) -> PtyDimensions {
        self.parser(terminal)
            .map(|parser| {
                let (rows, cols) = parser.screen().size();
                PtyDimensions { rows, cols }
            })
            .unwrap_or_default()
    }

    /// Connect *synchronously* and return whether the daemon resumed the exact
    /// previous client-scope/server-instance pair.
    ///
    /// Two callers remain, and both are deliberate: construction (which runs
    /// before the first frame is ever drawn) and [`Self::resume_and_resend`],
    /// which sits inside an already-synchronous correlated request whose
    /// idempotency key must be replayed on the very connection it re-establishes.
    /// Everything reachable from the render loop uses the connector thread
    /// instead.
    fn connect_inner(&mut self, spawn: SpawnPolicy) -> PtyResult<bool> {
        let resume = self.client_scope;
        let established = establish_connection(&self.socket_path, spawn, resume)?;
        Ok(self.install_connection(established, resume))
    }

    /// Start a background connection attempt, unless one is already running.
    fn start_connector(&mut self, spawn: SpawnPolicy) {
        if self.pending_connect.is_some() || self.connection.is_some() {
            return;
        }
        let socket_path = self.socket_path.clone();
        let resume = self.client_scope;
        let handle = thread::spawn(move || establish_connection(&socket_path, spawn, resume));
        self.pending_connect = Some(PendingConnect { handle, resume });
    }

    /// Collect a finished background connection attempt without ever blocking
    /// the caller. A still-running attempt is left alone for the next poll.
    fn poll_connector(&mut self) {
        let Some(pending) = self.pending_connect.as_ref() else {
            return;
        };
        if !pending.handle.is_finished() {
            return;
        }
        let pending = self
            .pending_connect
            .take()
            .expect("pending connector was just observed");
        let resume = pending.resume;
        match pending.handle.join() {
            Ok(Ok(established)) => {
                if self.connection.is_some() {
                    // A synchronous reconnect won the race; this socket is
                    // surplus and must not be leaked to the daemon.
                    let _ = established.reader.shutdown(Shutdown::Both);
                    return;
                }
                self.install_connection(established, resume);
                self.reset_retry_backoff();
            }
            Ok(Err(error)) => {
                self.report_disconnect(error.to_string());
                self.advance_retry_backoff();
            }
            Err(_) => {
                self.report_disconnect("the connection attempt panicked".to_string());
                self.advance_retry_backoff();
            }
        }
    }

    /// Adopt an established socket: reconcile the resumed scope, start the
    /// reader thread, and queue every tracked terminal for re-attachment.
    fn install_connection(
        &mut self,
        established: EstablishedConnection,
        requested_resume: Option<ClientScopeId>,
    ) -> bool {
        let EstablishedConnection {
            reader,
            writer: writer_stream,
            hello,
        } = established;
        let resumed_same = hello.resumed
            && requested_resume == Some(hello.client_scope)
            && self.server_instance == Some(hello.server_instance);
        if self.client_scope.is_some() && !resumed_same {
            self.pending_requests.clear();
            self.deferred_messages.clear();
            self.next_request_id = Some(RequestId::MIN);
            for pane in self.panes.values_mut() {
                pane.lease = None;
                pane.expected_output = None;
            }
        }
        self.client_scope = Some(hello.client_scope);
        self.server_instance = Some(hello.server_instance);

        let writer = Arc::new(Mutex::new(writer_stream));
        let (sender, receiver) = mpsc::sync_channel(SERVER_EVENT_QUEUE_CAPACITY);
        thread::spawn(move || {
            let mut reader = reader;
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
                            code: RejectCode::DaemonInternal,
                            message: format!("failed to read from mult-server: {error}"),
                        });
                        break;
                    }
                }
            }
        });
        self.connection = Some(ServerConnection { writer, receiver });
        self.disconnect_reported = false;
        self.enqueue_reattachments();
        resumed_same
    }

    /// Drop the current connection *and* shut its socket down in both
    /// directions.
    ///
    /// The reader thread owns its own `dup` of this socket and is parked in a
    /// blocking `read`. Dropping only the client's `writer` handle therefore
    /// leaves that thread parked forever on a still-open descriptor, so threads
    /// and file descriptors accumulate once per reconnect. `shutdown(Both)`
    /// applies to the socket rather than to one descriptor, so the parked read
    /// returns EOF and the thread exits. A poisoned writer mutex must not skip
    /// the shutdown: the lock only guards frame interleaving, and there is
    /// nothing left to interleave with.
    fn disconnect(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        let writer = match connection.writer.lock() {
            Ok(writer) => writer,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = writer.shutdown(Shutdown::Both);
    }

    fn write_idempotent_request(&mut self, message: &ClientMessage) -> PtyResult<()> {
        self.ensure_connected()?;
        match self.write(message) {
            Ok(()) => Ok(()),
            Err(error) if error.is_disconnected() => self.resume_and_resend(message),
            Err(error) => Err(error),
        }
    }

    fn resume_and_resend(&mut self, message: &ClientMessage) -> PtyResult<()> {
        let previous_scope = self.client_scope;
        let previous_instance = self.server_instance;
        self.disconnect();
        let resumed_same = self.connect_inner(SpawnPolicy::Autospawn)?;
        if !resumed_same
            || self.client_scope != previous_scope
            || self.server_instance != previous_instance
        {
            return Err(PtyError::ProtocolViolation(
                "mult-server identity changed; refusing to replay an unresolved request"
                    .to_string(),
            ));
        }
        self.write(message)
    }

    fn write_non_replayable(
        &mut self,
        message: &ClientMessage,
        operation: PtyDeliveryOperation,
        pane: PaneId,
    ) -> PtyResult<()> {
        let Some(connection) = &self.connection else {
            return Err(PtyError::Offline(
                "not connected to mult-server".to_string(),
            ));
        };
        let result = {
            let mut writer = connection.writer.lock().map_err(|_| {
                PtyError::Io(io::Error::other("server socket writer lock poisoned"))
            })?;
            write_non_replayable_frame(&mut *writer, message, operation, pane)
        };
        // Uncertain delivery is the one write failure that must not leave the
        // socket in use: the frame may be half on the wire.
        if result
            .as_ref()
            .is_err_and(|error| error.delivery().is_some())
        {
            self.disconnect();
        }
        result
    }

    fn write(&self, message: &ClientMessage) -> PtyResult<()> {
        let Some(connection) = &self.connection else {
            return Err(PtyError::Offline(
                "not connected to mult-server".to_string(),
            ));
        };
        let mut writer = connection
            .writer
            .lock()
            .map_err(|_| io::Error::other("server socket writer lock poisoned"))?;
        Ok(write_message(&mut *writer, message)?)
    }
}

struct AttemptTrackingWriter<'a, W: Write + ?Sized> {
    inner: &'a mut W,
    attempted: bool,
}

impl<W: Write + ?Sized> Write for AttemptTrackingWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !bytes.is_empty() {
            self.attempted = true;
        }
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn write_non_replayable_frame(
    writer: &mut (impl Write + ?Sized),
    message: &ClientMessage,
    operation: PtyDeliveryOperation,
    pane: PaneId,
) -> PtyResult<()> {
    let mut tracked = AttemptTrackingWriter {
        inner: writer,
        attempted: false,
    };
    match write_message(&mut tracked, message) {
        Ok(()) => Ok(()),
        // Nothing reached the socket, so this is an ordinary definite failure.
        Err(error) if !tracked.attempted => Err(PtyError::Io(error)),
        // Bytes were attempted and then failed: the daemon may have received
        // none, part, or all of the frame. Typed as uncertain so no layer above
        // can decide to retry terminal bytes on its own.
        Err(_) => Err(PtyError::DeliveryUncertain(PtyDeliveryError {
            operation,
            pane,
        })),
    }
}

impl Drop for PtyRuntime {
    fn drop(&mut self) {
        if let Some(connection) = &self.connection {
            if let Ok(mut writer) = connection.writer.lock() {
                for entry in self.panes.values() {
                    let (Some(pane), Some(lease)) = (entry.attachment, entry.lease) else {
                        continue;
                    };
                    let _ = write_message(&mut *writer, &ClientMessage::Detach { pane, lease });
                }
            }
        }
        // Release the reader thread as well: it is parked on a descriptor this
        // runtime no longer owns, and `shutdown` after the detach frames still
        // delivers them (only `SHUT_RD` discards, and only the receive queue).
        self.disconnect();
    }
}

/// PTY payload bytes a single server message will hand to a parser. Used to
/// budget one frame's parsing work in [`PtyRuntime::drain_events`].
fn server_message_output_bytes(message: &ServerMessage) -> usize {
    match message {
        ServerMessage::PtyOutput { bytes, .. } | ServerMessage::ReplayChunk { bytes, .. } => {
            bytes.len()
        }
        _ => 0,
    }
}

/// Connect, verify the peer, and complete the `Hello` exchange.
///
/// Every blocking step of connection establishment lives here so it can run on
/// either the constructing thread (before any frame exists) or a connector
/// thread — never on the render thread mid-session. It deliberately touches no
/// `PtyRuntime` state: adopting the result is [`PtyRuntime::install_connection`].
fn establish_connection(
    socket_path: &Path,
    spawn: SpawnPolicy,
    resume: Option<ClientScopeId>,
) -> PtyResult<EstablishedConnection> {
    let mut stream = if spawn.allows_spawn() {
        connect_or_spawn_server(socket_path)?
    } else {
        UnixStream::connect(socket_path)?
    };
    verify_peer_is_self(&stream, "mult-server")?;
    stream.set_nonblocking(false)?;
    let mut writer = stream.try_clone()?;
    write_message(
        &mut writer,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            resume,
        },
    )?;
    let hello = validate_server_hello_with_timeout(&mut stream, SERVER_HELLO_TIMEOUT)?;
    Ok(EstablishedConnection {
        reader: stream,
        writer,
        hello,
    })
}

impl PtyExit {
    pub fn label(&self) -> String {
        match &self.signal {
            Some(signal) => format!("terminated by {signal}"),
            None => format!("exit {}", self.code),
        }
    }
}

/// Queue an `Output` notification, folding it into the immediately preceding
/// one when the same terminal produced it. A busy pane arrives as a burst of
/// 8 KiB chunks within a single drain; the render loop only needs to know that
/// output happened and how much, so a burst costs one event, not one per chunk.
fn push_output_event(events: &mut Vec<PtyEvent>, terminal: PtyKey, byte_count: usize) {
    if let Some(PtyEvent::Output {
        terminal: previous_terminal,
        byte_count: previous_count,
    }) = events.last_mut()
    {
        if *previous_terminal == terminal {
            *previous_count = previous_count.saturating_add(byte_count);
            return;
        }
    }

    events.push(PtyEvent::Output {
        terminal,
        byte_count,
    });
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

/// Clear double-width characters that a narrowing resize would cut in half.
///
/// `fnug-vt100` narrows a row with `Row::resize`, which drops the trailing cells
/// but — unlike its own `Row::truncate` — leaves the `is_wide` flag on the cell
/// that becomes the last one. That cell then claims a second half that no
/// longer exists, and the next character printed there unwraps `None` inside
/// the emulator and panics (A14). Dragging a pane one column narrower with any
/// CJK text or emoji on screen is enough to reach it, and a panic here takes
/// down a UI holding the terminal in raw mode.
///
/// The repair erases the doomed character *before* the resize, while the grid
/// is still consistent: `EL` goes through `Row::erase`, which clears both
/// halves of a wide cell. Nothing is lost that the resize was not already going
/// to destroy — the character being erased is exactly the one whose second half
/// the truncation removes — and the work is skipped entirely unless a row
/// actually ends in a split wide character.
///
/// `DECSC`/`DECRC` bracket the repair because they are the only way to restore
/// the cursor *and* origin mode, and origin mode decides what the `CUP` below
/// addresses. That costs the pane's saved-cursor slot, which is acceptable
/// only because this runs during a resize, after which the child redraws from
/// scratch anyway.
///
/// **Known residual:** `Screen::set_size` resizes the alternate grid too, but
/// only the *current* grid is reachable through the public API, so an orphan
/// left in the inactive grid survives. Entering the alternate screen clears it
/// (`CSI ? 1049 h`), so the gap is one-directional: a pane narrowed while in
/// the alternate screen can still carry a split wide character back to the
/// normal grid on exit.
fn repair_wide_cells_before_narrowing(parser: &mut Parser, size: PtyDimensions) {
    let (rows, cols) = parser.screen().size();
    if size.cols >= cols {
        return;
    }
    // The floor guarantees at least two columns, so the surviving last column
    // is always a valid index and always has a cell before it.
    let last_col = size.cols.saturating_sub(1);

    // `Screen::cell` reads the *visible* rows, which are the drawing rows only
    // while the view is not scrolled back; the drawing rows are the ones
    // `set_size` truncates.
    let scrollback = parser.screen().scrollback();
    parser.set_scrollback(0);
    let split: Vec<u16> = (0..rows)
        .filter(|row| {
            parser
                .screen()
                .cell(*row, last_col)
                .is_some_and(vt100::Cell::is_wide)
        })
        .collect();

    if !split.is_empty() {
        let mut repair = Vec::with_capacity(split.len() * 12 + 8);
        // DECSC, then absolute (origin-mode-independent) addressing.
        repair.extend_from_slice(b"\x1b7\x1b[?6l");
        for row in split {
            // CUP is 1-based, so `last_col` addresses column `last_col + 1`,
            // and EL erases from there to the end of the row.
            repair
                .extend_from_slice(format!("\x1b[{};{}H\x1b[K", row + 1, last_col + 1).as_bytes());
        }
        repair.extend_from_slice(b"\x1b8");
        parser.process(&repair);
    }

    parser.set_scrollback(scrollback);
}

impl PtyMouseReport {
    /// The button this event is about, if it has one. Bare motion does not.
    fn button(self) -> Option<PtyMouseButton> {
        match self.action {
            PtyMouseAction::Press(button)
            | PtyMouseAction::Release(button)
            | PtyMouseAction::Drag(button) => Some(button),
            PtyMouseAction::Move => None,
        }
    }

    /// xterm's modifier bits. X10 (`MouseProtocolMode::Press`) predates them
    /// and carries none, so the caller passes the mode in rather than this
    /// deciding on its own.
    fn modifier_bits(self) -> u8 {
        let mut bits = 0;
        if self.shift {
            bits |= MOUSE_SHIFT_BIT;
        }
        if self.alt {
            bits |= MOUSE_ALT_BIT;
        }
        if self.ctrl {
            bits |= MOUSE_CTRL_BIT;
        }
        bits
    }
}

impl PtyMouseAction {
    /// Whether a program in `mode` asked to hear about this event at all.
    ///
    /// A wheel notch is always a press, so it survives every mode including
    /// press-only X10; a release does not exist in X10; and motion needs the
    /// mode that asked for it — with a button held (1002) or without (1003).
    ///
    /// This hangs off the action rather than off a whole [`PtyMouseReport`]
    /// because the routing layer has to ask the question *before* it has an
    /// event to send — see [`PtyRuntime::terminal_wants_mouse_action`].
    fn is_wanted_in(self, mode: MouseProtocolMode) -> bool {
        match self {
            PtyMouseAction::Press(_) => mode != MouseProtocolMode::None,
            PtyMouseAction::Release(button) => {
                !button.is_wheel()
                    && matches!(
                        mode,
                        MouseProtocolMode::PressRelease
                            | MouseProtocolMode::ButtonMotion
                            | MouseProtocolMode::AnyMotion
                    )
            }
            PtyMouseAction::Drag(button) => {
                !button.is_wheel()
                    && matches!(
                        mode,
                        MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
                    )
            }
            PtyMouseAction::Move => mode == MouseProtocolMode::AnyMotion,
        }
    }
}

/// Encode one mouse event for a program that has enabled mouse reporting, or
/// `None` when the program's mode does not want it.
///
/// The two families differ in more than syntax. SGR (DECSET 1006) reports the
/// button on release and marks the release with a lowercase `m`; the legacy
/// encodings have no room for either, so every release is the anonymous
/// "button 3" press-shaped report and the program is left to infer which
/// button it was. Getting that backwards leaves a program believing a button
/// is still held.
fn mouse_report_bytes(
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
    report: PtyMouseReport,
) -> Option<Vec<u8>> {
    if !report.action.is_wanted_in(mode) {
        return None;
    }

    let motion = match report.action {
        PtyMouseAction::Drag(_) | PtyMouseAction::Move => MOUSE_MOTION_BIT,
        _ => 0,
    };
    let modifiers = if mode == MouseProtocolMode::Press {
        0
    } else {
        report.modifier_bits()
    };
    let button = report
        .button()
        .map_or(MOUSE_BUTTON_NONE, |button| button.code())
        | motion
        | modifiers;
    let release = matches!(report.action, PtyMouseAction::Release(_));
    let legacy_button = if release {
        MOUSE_BUTTON_NONE | modifiers
    } else {
        button
    };

    Some(encode_mouse_event(
        encoding,
        MouseEventCode {
            button,
            legacy_button,
            release,
        },
        report.col.max(1),
        report.row.max(1),
    ))
}

/// The button field of one mouse report, in both of the forms the encodings
/// need. See [`mouse_report_bytes`] for why they differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MouseEventCode {
    button: u8,
    legacy_button: u8,
    release: bool,
}

/// Encode a single mouse event for a program that has enabled mouse
/// reporting. `col`/`row` are 1-based, screen-relative cell coordinates.
fn encode_mouse_event(
    encoding: MouseProtocolEncoding,
    code: MouseEventCode,
    col: u16,
    row: u16,
) -> Vec<u8> {
    match encoding {
        // SGR (DECSET 1006): `ESC [ < b ; x ; y M`, or `m` for a release.
        // Coordinates are unbounded here and the button survives the release,
        // which is why every modern program asks for this encoding.
        MouseProtocolEncoding::Sgr => {
            let final_char = if code.release { 'm' } else { 'M' };
            format!("\x1b[<{};{col};{row}{final_char}", code.button).into_bytes()
        }
        // UTF-8 (1005): `ESC [ M` then button and coordinates as `value + 32`,
        // each written as a UTF-8 code point.
        MouseProtocolEncoding::Utf8 => {
            let mut bytes = b"\x1b[M".to_vec();
            bytes.push(code.legacy_button.wrapping_add(32));
            push_utf8_mouse_coord(&mut bytes, col);
            push_utf8_mouse_coord(&mut bytes, row);
            bytes
        }
        // X10/Default: one byte per field as `value + 32`, so anything past
        // column/row 223 cannot be represented — clamp rather than wrap.
        MouseProtocolEncoding::Default => {
            let mut bytes = b"\x1b[M".to_vec();
            bytes.push(code.legacy_button.wrapping_add(32));
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

    /// Record the private modes this client implements itself, on the way past.
    ///
    /// Only DECSET/DECRST (`CSI ? … h` / `CSI ? … l`) is inspected, and only
    /// for [`FOCUS_REPORTING_MODE`] — every other mode is `vt100`'s business
    /// and is left to it. The sequence is still handed to the parser
    /// unchanged; this is an observation, not an interception.
    ///
    /// Called with the sequence's final byte while the parameters are still in
    /// [`Self::csi`], so it costs one scan of a short buffer and no allocation
    /// unless the sequence is a private mode change.
    fn observe_mode_change(&mut self, final_char: char) {
        let set = match final_char {
            'h' => true,
            'l' => false,
            _ => return,
        };
        let sequence = &self.csi[..self.csi_len];
        if !sequence.contains(&b'?') {
            return;
        }
        // `CSI ? 1004 ; 1006 h` sets both, so every parameter is checked.
        if parse_csi_params(sequence).contains(&FOCUS_REPORTING_MODE) {
            self.focus_reporting = set;
        }
    }

    /// Step the detector over one byte, reporting any query it completed.
    ///
    /// Deliberately free of the screen: a query's *content* may depend on it
    /// (the cursor position report does), but whether one was asked never does.
    /// Keeping the two apart is what lets [`feed_parser_with_responder`] scan a
    /// whole escape sequence before handing it to the parser.
    fn advance(&mut self, byte: u8) -> Option<TerminalQuery> {
        let (next, query) = match self.state {
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
                    Some(TerminalQuery::Device(PRIMARY_DEVICE_ATTRIBUTES_RESPONSE)),
                ),
                _ => (TerminalResponseState::Ground, None),
            },
            TerminalResponseState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.observe_mode_change(byte as char);
                    let query = csi_terminal_query(&self.csi[..self.csi_len], byte as char);
                    (TerminalResponseState::Ground, query)
                } else if self.csi_len >= TERMINAL_MAX_CSI_SEQUENCE_BYTES {
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
        query
    }
}

/// Neutralize control characters in text that did not come from the pane's own
/// program.
///
/// A `[mult]` system line carries strings the daemon supplied — a
/// `PtyEvent::Error` message, an `ExitInfo::signal` name — straight into the
/// emulator via `parser.process`. Left raw, an escape sequence inside one could
/// clear the screen, reposition the cursor, or repaint the pane to imitate
/// output that never happened. Every C0/C1 control and DEL becomes U+FFFD:
/// visible, inert, and length-preserving, so the spoof attempt shows up rather
/// than disappearing.
fn sanitize_system_line(message: &str) -> String {
    message
        .chars()
        .map(|ch| if ch.is_control() { '\u{fffd}' } else { ch })
        .collect()
}

/// Make a program-supplied window title safe to render as a label.
///
/// `vt100` stores an OSC 0/2 payload verbatim once it is valid UTF-8 — any
/// length, any control character — so neither bound can be assumed here.
///
/// The treatment deliberately differs from [`sanitize_system_line`]'s. That one
/// replaces each control with U+FFFD because its text is fed back *into an
/// emulator*, where a smuggled escape sequence could repaint the pane, and the
/// visible replacement is what makes the attempt show up. A title is only ever
/// drawn as text in a widget, cell by cell, so there is no sequence to
/// neutralize — only characters that would break the row it is drawn in. Those
/// are dropped, which is what a terminal does to a tab title, and it does not
/// disfigure the common legitimate case of a prompt escape leaving a stray
/// `\r` behind.
///
/// [`MAX_PANE_TITLE_CHARS`] is the second bound: the label is truncated to the
/// sidebar's width anyway, but this runs every frame, so a program that sets a
/// megabyte title must not cost a megabyte copy each time.
fn sanitize_pane_title(title: &str) -> String {
    title
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_PANE_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// An answer this client sends back to a program that queried the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalQueryResponse {
    kind: TerminalQueryKind,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalQueryKind {
    /// A cursor position report. Its content depends on where the cursor is,
    /// and within one chunk of output that does not change between the
    /// queries, so repeating it conveys nothing.
    CursorPosition,
    /// Device attributes or device status: a fixed answer.
    Device,
}

/// A query the detector recognised, before its answer is built. Separating this
/// from [`TerminalQueryResponse`] keeps the state machine independent of the
/// screen; only [`TerminalQuery::resolve`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalQuery {
    /// A fixed answer: device attributes or device status.
    Device(&'static [u8]),
    /// A cursor position report, private (DECXCPR) or not.
    CursorPosition { private: bool },
}

impl TerminalQuery {
    fn resolve(self, screen: &vt100::Screen) -> TerminalQueryResponse {
        match self {
            Self::Device(bytes) => TerminalQueryResponse {
                kind: TerminalQueryKind::Device,
                bytes: bytes.to_vec(),
            },
            Self::CursorPosition { private } => TerminalQueryResponse {
                kind: TerminalQueryKind::CursorPosition,
                bytes: cursor_position_report(screen, private),
            },
        }
    }
}

/// Drive the responding feed path over `chunks`, resizing to `sizes[i]` before
/// chunk `i`, and return the answer produced for each chunk.
///
/// The seam `fuzz/fuzz_targets/vt_response_detector.rs` needs: production code
/// reaches [`feed_parser_with_responder`] only from a daemon output event, and
/// the fuzz crate lives outside this workspace. Behind the `fuzzing` feature so
/// ordinary builds are byte-for-byte unaffected.
///
/// Sizes go through [`PtyDimensions::clamped`] exactly as a pane's do, so the
/// clamped floor is exercised on every run — the size the emulator used to
/// panic at (A13) — and interleaving resizes with output is what reaches the
/// narrowing case (A14).
#[cfg(feature = "fuzzing")]
pub fn fuzz_feed_terminal_output(sizes: &[(u16, u16)], chunks: &[&[u8]]) -> Vec<Vec<u8>> {
    let terminal = PtyKey::Terminal(TerminalId(0));
    let mut runtime = PtyRuntime::new_offline();
    runtime.ensure_parser(terminal, PtyDimensions::default());
    let mut responder = TerminalResponseDetector::default();

    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            if let Some(&(rows, cols)) = sizes.get(index % sizes.len().max(1)) {
                runtime.resize_parser(terminal, PtyDimensions { rows, cols });
            }
            let parser = runtime
                .parser_mut(terminal)
                .expect("parser was just ensured");
            let answer = feed_parser_with_responder(parser, &mut responder, chunk);
            clamp_parser_scrollback(parser);
            answer
        })
        .collect()
}

/// Feed `bytes` to `parser` while letting `responder` answer terminal queries,
/// returning the answers coalesced into a single input payload.
///
/// The parser is driven in two kinds of batch, never byte by byte. While the
/// responder is idle (Ground) no query can begin until an escape, so the run of
/// printable bytes up to the next ESC goes in one `process` call. From the ESC
/// onwards the responder scans ahead — it is a pure state machine — until the
/// sequence ends or the chunk does, and that whole sequence then goes in one
/// `process` call too. Escape-dense output is the normal case for a TUI child,
/// and it used to pay a full `vte` dispatch per byte.
///
/// The carve-out that made the old code per-byte is preserved: a cursor
/// position report must describe the screen *at the query point*. Batching one
/// sequence at a time keeps that exact, because the only sequences that produce
/// an answer — `CSI c`, `CSI n`, `ESC Z` — are queries, and a query never moves
/// the cursor or touches the grid. The screen the parser holds after the
/// sequence is therefore the screen it held at the query point, and answers
/// still come out in stream order. Nothing may be batched *across* a sequence
/// boundary: `CSI 3;4H` followed by `CSI 6n` would then report the wrong
/// position. `batched_feed_matches_byte_at_a_time_feed_on_*` pins all of this
/// against an explicit byte-at-a-time reference.
///
/// The answers are bounded, because a chunk of PTY output is chosen by whatever
/// is running in the pane: 8 KiB of `\x1b[6n` used to become ~2048 separate
/// `Input` messages, each a blocking socket write on the render thread. At most
/// one cursor report and [`MAX_TERMINAL_QUERY_RESPONSES_PER_CHUNK`] answers in
/// total are produced per chunk, and the payload itself is capped.
fn feed_parser_with_responder(
    parser: &mut Parser,
    responder: &mut TerminalResponseDetector,
    bytes: &[u8],
) -> Vec<u8> {
    let mut payload = Vec::new();
    let mut answered = 0_usize;
    let mut cursor_reported = false;
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

        // One escape sequence: scan it out of the chunk, then hand the parser
        // all of it at once. An unterminated sequence simply runs to the end of
        // the chunk and the responder resumes mid-sequence on the next one.
        let start = index;
        let mut query = None;
        while index < bytes.len() {
            let recognised = responder.advance(bytes[index]);
            index += 1;
            if recognised.is_some() {
                query = recognised;
            }
            if responder.is_ground() {
                break;
            }
        }
        parser.process(&bytes[start..index]);

        if let Some(query) = query {
            let response = query.resolve(parser.screen());
            let repeated_cursor =
                cursor_reported && response.kind == TerminalQueryKind::CursorPosition;
            let over_budget = answered >= MAX_TERMINAL_QUERY_RESPONSES_PER_CHUNK
                || payload.len() + response.bytes.len() > MAX_TERMINAL_QUERY_RESPONSE_BYTES;
            if !repeated_cursor && !over_budget {
                cursor_reported |= response.kind == TerminalQueryKind::CursorPosition;
                answered += 1;
                payload.extend_from_slice(&response.bytes);
            }
        }
    }
    payload
}

fn csi_terminal_query(sequence: &[u8], final_char: char) -> Option<TerminalQuery> {
    let private = sequence.contains(&b'?');
    let params = parse_csi_params(sequence);
    match final_char {
        'c' if !private && param_or_default(&params, 0, 0) == 0 => {
            Some(TerminalQuery::Device(PRIMARY_DEVICE_ATTRIBUTES_RESPONSE))
        }
        'n' if !private => match param_or_default(&params, 0, 0) {
            5 => Some(TerminalQuery::Device(DEVICE_STATUS_OK_RESPONSE)),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ServerHello {
    server_instance: ServerInstanceId,
    client_scope: ClientScopeId,
    resumed: bool,
}

fn validate_server_hello_with_timeout(
    stream: &mut UnixStream,
    timeout: Duration,
) -> PtyResult<ServerHello> {
    stream.set_read_timeout(Some(timeout))?;
    let result = validate_server_hello(stream);
    let reset_result = stream.set_read_timeout(None);

    match result {
        Ok(hello) => {
            reset_result?;
            Ok(hello)
        }
        Err(error) => Err(map_server_hello_error(error, timeout)),
    }
}

fn map_server_hello_error(error: PtyError, timeout: Duration) -> PtyError {
    match &error {
        PtyError::Io(io_error)
            if matches!(
                io_error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            PtyError::Timeout(format!(
                "timed out after {timeout:?} waiting for mult-server hello"
            ))
        }
        _ => error,
    }
}

fn validate_server_hello(reader: &mut impl io::Read) -> PtyResult<ServerHello> {
    match read_message::<ServerMessage>(reader)? {
        ServerMessage::Hello {
            protocol_version,
            server_instance,
            client_scope,
            resumed,
        } if protocol_version == PROTOCOL_VERSION => Ok(ServerHello {
            server_instance,
            client_scope,
            resumed,
        }),
        // A version disagreement is the one connection failure with a concrete
        // remedy, so it is classified rather than merged into "invalid data":
        // the status surface can tell the user to restart the daemon.
        ServerMessage::Hello {
            protocol_version, ..
        } => Err(PtyError::Rejected {
            code: RejectCode::ProtocolMismatch,
            detail: format!(
                "mult-server protocol version {protocol_version} is incompatible with client version {PROTOCOL_VERSION}; restart mult-server"
            ),
        }),
        ServerMessage::Error { code, message } => Err(PtyError::Rejected {
            code,
            detail: message,
        }),
        message => Err(PtyError::ProtocolViolation(format!(
            "unexpected mult-server hello response: {message:?}"
        ))),
    }
}

fn message_request_id(message: &ServerMessage) -> Option<RequestId> {
    match message {
        ServerMessage::CreateResult { request_id, .. }
        | ServerMessage::AttachResult { request_id, .. }
        | ServerMessage::ReplayBegin { request_id, .. }
        | ServerMessage::ReplayChunk { request_id, .. }
        | ServerMessage::ReplayEnd { request_id, .. }
        | ServerMessage::StopResult { request_id, .. }
        | ServerMessage::AgentStatusResult { request_id, .. } => Some(*request_id),
        _ => None,
    }
}

fn protocol_order_error(message: &str) -> PtyError {
    // Keep the terminal/session mapping but mark this attach unreconciled. A
    // later explicit attach can rebuild from an authoritative replay.
    PtyError::ProtocolViolation(message.to_string())
}

/// Every daemon refusal becomes a [`PtyError::Rejected`] carrying the wire's own
/// [`RejectCode`]. The `detail` string is only ever rendered — control flow
/// switches on the code, so rewording any of these cannot change behaviour.
fn create_error(error: CreateError) -> PtyError {
    let detail = match &error {
        CreateError::SessionAlreadyExists { session }
        | CreateError::IdentityAlreadyExists { session } => {
            format!("session {} already exists", session.id.0)
        }
        CreateError::IdentityMismatch { session, mismatch } => {
            format!("session {} identity mismatch: {mismatch:?}", session.0)
        }
        CreateError::InvalidAgentMetadata(error) => format!("agent metadata rejected: {error:?}"),
        CreateError::RequestCollision => "create request ID collision".to_string(),
        CreateError::RetryExpired => "create request retry expired".to_string(),
        CreateError::Failed { message, .. } => message.clone(),
    };
    PtyError::Rejected {
        code: error.code(),
        detail,
    }
}

fn attach_error(error: AttachError) -> PtyError {
    let detail = match &error {
        AttachError::SessionNotFound { session } => {
            format!("server session {} is unavailable", session.0)
        }
        AttachError::IdentityMismatch { session, mismatch } => format!(
            "server session {} identity mismatch: {mismatch:?}",
            session.0
        ),
        AttachError::Superseded => "attachment request was superseded by a takeover".to_string(),
        AttachError::RequestCollision => "attach request ID collision".to_string(),
        AttachError::RetryExpired => "attach request retry expired".to_string(),
        AttachError::Failed { message, .. } => message.clone(),
    };
    PtyError::Rejected {
        code: error.code(),
        detail,
    }
}

fn stop_error(error: StopError) -> PtyError {
    let detail = match &error {
        StopError::RequestCollision => "stop request ID collision".to_string(),
        StopError::RetryExpired => "stop request retry expired".to_string(),
        StopError::IdentityMismatch { pane, mismatch } => {
            format!("pane {} identity mismatch: {mismatch:?}", pane.0)
        }
        StopError::LeaseRejected(reason) => format!("stop lease rejected: {reason:?}"),
        StopError::Failed { message, .. } => message.clone(),
    };
    PtyError::Rejected {
        code: error.code(),
        detail,
    }
}

fn agent_status_error(error: AgentStatusError) -> PtyError {
    PtyError::Rejected {
        code: error.code(),
        detail: format!("agent status rejected: {error:?}"),
    }
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

/// Environment variables an autospawned daemon may inherit.
///
/// The daemon outlives the client that started it and passes its own
/// environment to every PTY it later spawns — for *every* client that connects
/// afterwards. Inheriting this client's full environment therefore takes
/// whatever secrets happen to be exported in the first shell that ever ran
/// `mult` (API keys, tokens, agent credentials) and re-exports them into every
/// pane of every later session. Only what a daemon and its login shells
/// genuinely need is forwarded.
const SERVER_ENV_ALLOW_LIST: &[&str] =
    &["PATH", "HOME", "SHELL", "USER", "LOGNAME", "TERM", "LANG"];
/// Prefixes forwarded wholesale: locale categories, and `mult`'s own settings
/// (including `MULT_SOCKET_PATH`, which is also set explicitly below).
const SERVER_ENV_ALLOW_PREFIXES: &[&str] = &["LC_", "MULT_"];

fn spawn_server(socket_path: &Path) -> io::Result<()> {
    let server = server_executable().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate a trusted mult-server next to the mult executable; run `mult-server` manually",
        )
    })?;

    let mut command = Command::new(server);
    command.env_clear();
    for (key, value) in env::vars_os() {
        if server_env_is_allowed(&key) {
            command.env(key, value);
        }
    }
    command
        .env(SOCKET_PATH_ENV, socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_autospawned_server(&mut command);
    command.spawn().map(|_| ())
}

fn server_env_is_allowed(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    SERVER_ENV_ALLOW_LIST.contains(&key)
        || SERVER_ENV_ALLOW_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
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

fn wait_for_server(path: &Path) -> io::Result<UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(2);
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

fn server_executable() -> Option<PathBuf> {
    let mut path = env::current_exe().ok()?;
    let stem = path.file_stem()?.to_str()?;
    if stem != "mult" {
        return None;
    }

    path.set_file_name(server_executable_name());
    is_trusted_executable(&path).then_some(path)
}

/// Whether a binary this client is about to *execute* is beyond the reach of
/// other local users.
///
/// Autospawn resolves the daemon purely by filename next to `current_exe()`, so
/// the check is on the file it will actually run: a regular file owned by this
/// user or by root, with no group/other write bit. The parent directory is
/// checked the same way — a writable directory means the name (or a symlink
/// standing in for it) can simply be replaced, which is the same attack one
/// level up. Symlinks are followed on purpose: packaged installs (Nix profiles,
/// `cargo install` shims) legitimately link into a store, and both the link's
/// directory and the target are validated.
fn is_trusted_executable(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let owner_is_trusted = |uid: u32| uid == effective_uid() || uid == 0;
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file()
        || !owner_is_trusted(metadata.uid())
        || metadata.mode() & 0o022 != 0
    {
        return false;
    }

    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(parent_metadata) = fs::metadata(parent) else {
        return false;
    };
    parent_metadata.file_type().is_dir()
        && owner_is_trusted(parent_metadata.uid())
        && parent_metadata.mode() & 0o022 == 0
}

fn server_executable_name() -> &'static str {
    if cfg!(windows) {
        "mult-server.exe"
    } else {
        "mult-server"
    }
}

fn wire_session_identity(identity: SessionIdentity) -> WireSessionIdentity {
    WireSessionIdentity {
        namespace: wire_state_namespace(identity.namespace),
        token: mult_protocol::SessionToken::from_bytes(identity.token.as_bytes())
            .expect("durable model tokens are non-zero"),
    }
}

fn wire_state_namespace(namespace: StateNamespace) -> WireStateNamespace {
    WireStateNamespace::from_bytes(namespace.as_bytes())
        .expect("durable model namespaces are non-zero")
}

#[cfg(test)]
fn test_wire_session_identity(key: PtyKey) -> WireSessionIdentity {
    let mut token = [0_u8; 16];
    token[8..].copy_from_slice(&wire_id(key).to_be_bytes());
    if token == [0; 16] {
        token[15] = 1;
    }
    WireSessionIdentity {
        namespace: WireStateNamespace::from_bytes([0xa1; 16]).unwrap(),
        token: mult_protocol::SessionToken::from_bytes(token).unwrap(),
    }
}

fn session_for_key(key: PtyKey) -> SessionId {
    SessionId(wire_id(key))
}

fn pane_for_key(key: PtyKey) -> PaneId {
    PaneId(wire_id(key))
}

/// The on-the-wire session/pane id for a PTY key. Durable terminals keep their
/// raw id; chat-agent PTYs set the high bit. This is the only place the old
/// high-bit encoding lives now, and it is kept identical so the daemon (which
/// is keyed purely by these ids) is unaffected by the `PtyKey` change.
fn wire_id(key: PtyKey) -> u64 {
    match key {
        PtyKey::Terminal(terminal) => terminal.0,
        PtyKey::ChatAgent(chat) => chat.0 | RUNTIME_TERMINAL_ID_FLAG,
    }
}

/// Inverse of `wire_id`: recover the `PtyKey` for a pane id received from the
/// server (used for output on panes the client has not explicitly registered).
fn key_for_pane_id(pane: PaneId) -> PtyKey {
    if pane.0 & RUNTIME_TERMINAL_ID_FLAG != 0 {
        PtyKey::ChatAgent(ChatId(pane.0 & !RUNTIME_TERMINAL_ID_FLAG))
    } else {
        PtyKey::Terminal(TerminalId(pane.0))
    }
}

fn launch_spec(spawn: &PtySpawn) -> LaunchSpec {
    spawn
        .args
        .last()
        .cloned()
        .map(LaunchSpec::Command)
        .unwrap_or(LaunchSpec::Shell)
}

fn session_name(spawn: &PtySpawn, launch: &LaunchSpec) -> String {
    match launch {
        LaunchSpec::Shell => format!("shell {}", wire_id(spawn.terminal)),
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

    const TEST_IO_TIMEOUT: Duration = Duration::from_secs(2);

    fn test_request_id(value: u64) -> RequestId {
        RequestId::new(value).expect("non-zero request ID")
    }

    fn test_lease() -> AttachmentLease {
        AttachmentLease::MIN
    }

    fn test_scope() -> ClientScopeId {
        ClientScopeId::from_bytes([2; 16])
    }

    fn test_server_instance() -> ServerInstanceId {
        ServerInstanceId::from_bytes([1; 16])
    }

    fn read_client_message(stream: &mut UnixStream, operation: &str) -> ClientMessage {
        stream
            .set_read_timeout(Some(TEST_IO_TIMEOUT))
            .unwrap_or_else(|error| panic!("set timeout while {operation}: {error}"));
        read_message(stream).unwrap_or_else(|error| {
            panic!("timed out or failed after {TEST_IO_TIMEOUT:?} while {operation}: {error}")
        })
    }

    #[test]
    fn pty_spawn_uses_default_size() {
        let spawn = PtySpawn::shell(PtyKey::Terminal(TerminalId(7)), None, BTreeMap::new());

        assert_eq!(spawn.terminal, PtyKey::Terminal(TerminalId(7)));
        assert_eq!(spawn.args, Vec::<String>::new());
        assert_eq!(spawn.size, PtyDimensions { rows: 24, cols: 80 });
        assert!(!spawn.program.is_empty());
    }

    #[test]
    fn pty_spawn_command_line_runs_through_shell() {
        let spawn = PtySpawn::command_line(
            PtyKey::Terminal(TerminalId(7)),
            "cargo test".to_string(),
            None,
            BTreeMap::new(),
        );

        assert_eq!(spawn.terminal, PtyKey::Terminal(TerminalId(7)));
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
        let terminal = PtyKey::Terminal(TerminalId(9));
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree");

        assert_eq!(
            runtime.terminal_lines(terminal),
            vec!["two".to_string(), "three".to_string()]
        );
        assert!(runtime.parser(terminal).is_some());
        assert!(!runtime.terminal_output_is_blank(terminal));
    }

    #[test]
    fn parser_resize_updates_screen_size() {
        let mut runtime = PtyRuntime::new_offline();
        let terminal = PtyKey::Terminal(TerminalId(9));

        runtime
            .resize(terminal, PtyDimensions { rows: 5, cols: 12 })
            .expect("resize parser");

        assert_eq!(runtime.parser(terminal).unwrap().screen().size(), (5, 12));
    }

    #[test]
    fn send_paste_wraps_when_parser_reports_bracketed_paste() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.pane_entry(terminal).identity = Some(test_wire_session_identity(terminal));
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"\x1b[?2004h");

        assert!(runtime.send_paste(terminal, "one\ntwo").expect("paste"));

        let message = read_client_message(&mut server_stream, "reading paste input");
        assert_eq!(
            message,
            ClientMessage::Paste {
                pane,
                lease: test_lease(),
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
                server_instance: test_server_instance(),
                client_scope: test_scope(),
                resumed: false,
            },
        )
        .expect("write hello");

        let error = validate_server_hello(&mut bytes.as_slice()).expect_err("reject version");

        // F8: a version disagreement is a code, not a shade of "invalid data".
        assert_eq!(error.code(), Some(RejectCode::ProtocolMismatch));
        assert!(error.to_string().contains("restart mult-server"));
    }

    #[test]
    fn validate_server_hello_times_out_when_peer_is_silent() {
        let (mut client, _server) = UnixStream::pair().expect("create socket pair");

        let error = validate_server_hello_with_timeout(&mut client, Duration::from_millis(10))
            .expect_err("silent peer should time out");

        assert!(matches!(error, PtyError::Timeout(_)));
        assert!(error.to_string().contains("waiting for mult-server hello"));
    }

    #[test]
    fn peer_owner_check_accepts_same_user_socket_pair() {
        let (client, _server) = UnixStream::pair().expect("create socket pair");

        verify_peer_is_self(&client, "test peer").expect("same uid peer is accepted");
    }

    #[test]
    fn attach_existing_sends_only_attach_and_marks_terminal_running() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::sync_channel(8);
        let terminal = PtyKey::Terminal(TerminalId(7));
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        let server = thread::spawn(move || {
            let message = read_client_message(&mut server_stream, "reading restoration Attach");
            let ClientMessage::Attach {
                request_id,
                session,
                rows,
                cols,
                ..
            } = message
            else {
                panic!("expected Attach");
            };
            assert_eq!((session, rows, cols), (SessionId(7), 6, 20));
            let lease = test_lease();
            sender
                .send(ServerMessage::AttachResult {
                    request_id,
                    outcome: AttachOutcome::Attached {
                        session,
                        pane: mult_protocol::PaneInfo {
                            id: PaneId(7),
                            title: "test".to_string(),
                            rows,
                            cols,
                        },
                        lease,
                    },
                })
                .expect("send attach confirmation");
            sender
                .send(ServerMessage::ReplayBegin {
                    request_id,
                    pane: PaneId(7),
                    lease,
                    first_sequence: OutputSequence::ZERO,
                    watermark: OutputSequence::ZERO,
                    omitted_prefix_bytes: 0,
                })
                .expect("send replay begin");
            sender
                .send(ServerMessage::ReplayEnd {
                    request_id,
                    pane: PaneId(7),
                    lease,
                    watermark: OutputSequence::ZERO,
                })
                .expect("send replay end");
        });

        let result = runtime
            .attach_existing(terminal, PtyDimensions { rows: 6, cols: 20 })
            .expect("attach existing session");

        assert_eq!(result, AttachExistingResult::Attached);
        assert!(runtime.is_running(terminal));
        server.join().expect("server thread should finish");
    }

    #[test]
    fn registered_durable_identity_is_carried_by_attach_without_create() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("socket pair");
        let (sender, receiver) = mpsc::sync_channel(8);
        let terminal = PtyKey::Terminal(TerminalId(12));
        let identity = test_model_session_identity(0x31, 0x32);
        let expected = wire_session_identity(identity);
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        runtime
            .register_session_identity(terminal, identity)
            .expect("register identity");
        let server = thread::spawn(move || {
            let ClientMessage::Attach {
                request_id,
                identity,
                session,
                ..
            } = read_client_message(&mut server_stream, "identity Attach")
            else {
                panic!("expected Attach")
            };
            assert_eq!(identity, expected);
            sender
                .send(ServerMessage::AttachResult {
                    request_id,
                    outcome: AttachOutcome::Error(AttachError::SessionNotFound { session }),
                })
                .unwrap();
        });

        assert_eq!(
            runtime
                .attach_existing(terminal, PtyDimensions::default())
                .unwrap(),
            AttachExistingResult::Missing
        );
        server.join().unwrap();
    }

    #[test]
    fn agent_status_adapter_round_trips_update_and_reconnect_query() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("socket pair");
        let (sender, receiver) = mpsc::sync_channel(8);
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        let identity = test_wire_session_identity(PtyKey::ChatAgent(ChatId(9)));
        let generation = mult_protocol::AgentGeneration::from_bytes([0x71; 16]).unwrap();
        let record = AgentStatusRecord {
            schema_version: mult_protocol::AGENT_STATUS_SCHEMA_VERSION,
            identity,
            chat_id: 9,
            agent: mult_protocol::AgentKind::Pi,
            generation,
            status: mult_protocol::AgentStatus::Running,
        };
        let server = thread::spawn(move || {
            let ClientMessage::UpdateAgentStatus { request_id, record } =
                read_client_message(&mut server_stream, "status update")
            else {
                panic!("expected status update")
            };
            sender
                .send(ServerMessage::AgentStatusResult {
                    request_id,
                    outcome: AgentStatusOutcome::Updated(record),
                })
                .unwrap();
            let ClientMessage::GetAgentStatus { request_id, .. } =
                read_client_message(&mut server_stream, "status query")
            else {
                panic!("expected status query")
            };
            sender
                .send(ServerMessage::AgentStatusResult {
                    request_id,
                    outcome: AgentStatusOutcome::Current(Some(record)),
                })
                .unwrap();
        });

        assert_eq!(runtime.update_agent_status(record).unwrap(), record);
        assert_eq!(
            runtime
                .get_agent_status(AgentStatusQuery {
                    schema_version: mult_protocol::AGENT_STATUS_SCHEMA_VERSION,
                    identity,
                    chat_id: 9,
                    agent: mult_protocol::AgentKind::Pi,
                    generation: record.generation,
                })
                .unwrap(),
            Some(record)
        );
        server.join().unwrap();
    }

    #[test]
    fn attach_replay_is_contiguous_through_the_live_watermark() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("socket pair");
        let (sender, receiver) = mpsc::sync_channel(16);
        let terminal = PtyKey::Terminal(TerminalId(11));
        let pane = PaneId(11);
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        let server = thread::spawn(move || {
            let ClientMessage::Attach { request_id, .. } =
                read_client_message(&mut server_stream, "reading ordered Attach")
            else {
                panic!("expected Attach");
            };
            let lease = test_lease();
            sender
                .send(ServerMessage::AttachResult {
                    request_id,
                    outcome: AttachOutcome::Attached {
                        session: SessionId(11),
                        pane: mult_protocol::PaneInfo {
                            id: pane,
                            title: "ordered".to_string(),
                            rows: 2,
                            cols: 20,
                        },
                        lease,
                    },
                })
                .unwrap();
            sender
                .send(ServerMessage::ReplayBegin {
                    request_id,
                    pane,
                    lease,
                    first_sequence: OutputSequence::new(5),
                    watermark: OutputSequence::new(11),
                    omitted_prefix_bytes: 5,
                })
                .unwrap();
            for (sequence, bytes) in [
                (5, Arc::<[u8]>::from(&b"abc"[..])),
                (8, Arc::<[u8]>::from(&b"def"[..])),
            ] {
                sender
                    .send(ServerMessage::ReplayChunk {
                        request_id,
                        pane,
                        lease,
                        sequence: OutputSequence::new(sequence),
                        bytes,
                    })
                    .unwrap();
            }
            sender
                .send(ServerMessage::ReplayEnd {
                    request_id,
                    pane,
                    lease,
                    watermark: OutputSequence::new(11),
                })
                .unwrap();
            sender
                .send(ServerMessage::PtyOutput {
                    pane,
                    lease,
                    sequence: OutputSequence::new(11),
                    bytes: Arc::from(b"!".to_vec()),
                })
                .unwrap();
        });

        assert_eq!(
            runtime
                .attach_existing(terminal, PtyDimensions { rows: 2, cols: 20 })
                .expect("ordered attach"),
            AttachExistingResult::Attached
        );
        server.join().unwrap();
        let events = runtime.drain_events();

        assert!(events.iter().any(|event| matches!(
            event,
            PtyEvent::ReplayTruncated {
                terminal: event_terminal,
                omitted_bytes: 5,
            } if *event_terminal == terminal
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PtyEvent::Output {
                terminal: event_terminal,
                byte_count: 1,
            } if *event_terminal == terminal
        )));
        assert!(runtime.terminal_lines(terminal)[0].contains("abcdef!"));
        assert_eq!(
            runtime.pane_entry(terminal).expected_output,
            Some(OutputSequence::new(12))
        );
    }

    #[test]
    fn attach_existing_reports_missing_without_creating_a_session() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::sync_channel(8);
        let terminal = PtyKey::Terminal(TerminalId(8));
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        let server = thread::spawn(move || {
            let message = read_client_message(&mut server_stream, "reading missing-session Attach");
            let ClientMessage::Attach { request_id, .. } = message else {
                panic!("expected Attach");
            };
            sender
                .send(ServerMessage::AttachResult {
                    request_id,
                    outcome: AttachOutcome::Error(AttachError::SessionNotFound {
                        session: SessionId(8),
                    }),
                })
                .expect("send missing-session response");
        });

        let result = runtime
            .attach_existing(terminal, PtyDimensions::default())
            .expect("missing session is a recoverable result");

        assert_eq!(result, AttachExistingResult::Missing);
        assert!(!runtime.is_running(terminal));
        server.join().expect("server thread should finish");
    }

    #[test]
    fn start_rolls_back_local_attachment_when_attach_is_rejected() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::sync_channel(8);
        let (completed_tx, completed_rx) = mpsc::sync_channel(1);
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        let server = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let create = read_client_message(&mut server_stream, "reading CreateSession");
                let ClientMessage::CreateSession {
                    request_id: create_id,
                    requested_id: Some(session),
                    ..
                } = create
                else {
                    panic!("expected requested CreateSession");
                };
                sender
                    .send(ServerMessage::CreateResult {
                        request_id: create_id,
                        outcome: CreateOutcome::Created {
                            session: mult_protocol::SessionInfo {
                                id: session,
                                identity: test_wire_session_identity(terminal),
                                name: "test".to_string(),
                                pane: PaneId(7),
                                attached: false,
                            },
                        },
                    })
                    .expect("send create result");
                let attach = read_client_message(&mut server_stream, "reading Attach");
                let ClientMessage::Attach {
                    request_id: attach_id,
                    session,
                    rows,
                    cols,
                    ..
                } = attach
                else {
                    panic!("expected Attach");
                };
                assert_eq!((session, rows, cols), (SessionId(7), 24, 80));
                sender
                    .send(ServerMessage::AttachResult {
                        request_id: attach_id,
                        outcome: AttachOutcome::Error(AttachError::Superseded),
                    })
                    .expect("send attach rejection");
            }));
            let _ = completed_tx.send(());
            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
        });

        let error = runtime
            .start(PtySpawn::shell(terminal, None, BTreeMap::new()))
            .expect_err("attach rejection should fail start");

        assert_eq!(error.code(), Some(RejectCode::Superseded));
        assert!(!runtime.is_running(terminal));
        assert_eq!(runtime.key_for_pane(pane), None);
        completed_rx
            .recv_timeout(TEST_IO_TIMEOUT)
            .expect("server thread should complete within the test timeout");
        server.join().expect("server thread should finish");
    }

    #[test]
    fn pty_stop_sends_stop_message_and_clears_local_attachment() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        sender
            .send(ServerMessage::StopResult {
                request_id: RequestId::MIN,
                outcome: StopOutcome::Stopped {
                    exit: mult_protocol::ExitInfo {
                        code: 0,
                        signal: None,
                    },
                },
            })
            .expect("send stop confirmation");

        assert!(runtime.stop(terminal).expect("stop terminal"));

        let message = read_client_message(&mut server_stream, "reading Stop");
        assert_eq!(
            message,
            ClientMessage::Stop {
                request_id: RequestId::MIN,
                identity: test_wire_session_identity(terminal),
                pane,
                lease: test_lease(),
            }
        );
        assert!(!runtime.is_running(terminal));
        assert_eq!(runtime.key_for_pane(pane), None);
    }

    #[test]
    fn pty_stop_keeps_local_attachment_when_server_rejects_stop() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        sender
            .send(ServerMessage::StopResult {
                request_id: RequestId::MIN,
                outcome: StopOutcome::Error(StopError::Failed {
                    code: RejectCode::DaemonInternal,
                    message: "failed to kill child".to_string(),
                }),
            })
            .expect("send stop rejection");

        let error = runtime.stop(terminal).expect_err("stop should fail");

        assert_eq!(error.code(), Some(RejectCode::DaemonInternal));
        assert!(error.to_string().contains("failed to kill child"));
        let message = read_client_message(&mut server_stream, "reading Stop");
        assert_eq!(
            message,
            ClientMessage::Stop {
                request_id: RequestId::MIN,
                identity: test_wire_session_identity(terminal),
                pane,
                lease: test_lease(),
            }
        );
        assert!(runtime.is_running(terminal));
        assert_eq!(runtime.key_for_pane(pane), Some(terminal));
    }

    #[test]
    fn input_returns_scrolled_parser_to_bottom() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree");
        assert!(runtime.scroll_up(terminal, 1).expect("scroll up"));
        assert!(runtime.parser(terminal).unwrap().screen().scrollback() > 0);

        assert!(runtime.send_input(terminal, b"x").expect("send input"));

        assert_eq!(runtime.parser(terminal).unwrap().screen().scrollback(), 0);
        let message = read_client_message(&mut server_stream, "reading input");
        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                lease: test_lease(),
                bytes: b"x".to_vec(),
            }
        );
    }

    /// A press of `button` at 12,5 with no modifier held.
    fn press_at(button: PtyMouseButton, col: u16, row: u16) -> PtyMouseReport {
        PtyMouseReport {
            action: PtyMouseAction::Press(button),
            col,
            row,
            shift: false,
            alt: false,
            ctrl: false,
        }
    }

    fn sgr_bytes(mode: MouseProtocolMode, report: PtyMouseReport) -> Option<String> {
        mouse_report_bytes(mode, MouseProtocolEncoding::Sgr, report)
            .map(|bytes| String::from_utf8(bytes).expect("SGR reports are ASCII"))
    }

    #[test]
    fn encode_mouse_event_covers_each_protocol_encoding() {
        let wheel_up = MouseEventCode {
            button: WHEEL_UP_BUTTON,
            legacy_button: WHEEL_UP_BUTTON,
            release: false,
        };
        let wheel_down = MouseEventCode {
            button: WHEEL_DOWN_BUTTON,
            legacy_button: WHEEL_DOWN_BUTTON,
            release: false,
        };
        // SGR: human-readable decimal coordinates, terminated with `M`.
        assert_eq!(
            encode_mouse_event(MouseProtocolEncoding::Sgr, wheel_up, 12, 5),
            b"\x1b[<64;12;5M".to_vec()
        );
        // X10/Default: one byte per field as `value + 32`, clamped at 223.
        assert_eq!(
            encode_mouse_event(MouseProtocolEncoding::Default, wheel_down, 1, 1),
            vec![0x1b, b'[', b'M', 65 + 32, 1 + 32, 1 + 32]
        );
        assert_eq!(
            encode_mouse_event(MouseProtocolEncoding::Default, wheel_up, 1000, 1),
            vec![0x1b, b'[', b'M', 64 + 32, 223 + 32, 1 + 32]
        );
        // UTF-8: `value + 32` written as a code point (multi-byte past 223).
        assert_eq!(
            encode_mouse_event(MouseProtocolEncoding::Utf8, wheel_up, 300, 1),
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
    fn mouse_reports_encode_every_button_motion_and_modifier() {
        let mode = MouseProtocolMode::AnyMotion;
        // Buttons are numbered from zero; the wheel sets bit 6.
        assert_eq!(
            sgr_bytes(mode, press_at(PtyMouseButton::Left, 12, 5)).as_deref(),
            Some("\x1b[<0;12;5M")
        );
        assert_eq!(
            sgr_bytes(mode, press_at(PtyMouseButton::Middle, 1, 1)).as_deref(),
            Some("\x1b[<1;1;1M")
        );
        assert_eq!(
            sgr_bytes(mode, press_at(PtyMouseButton::Right, 1, 1)).as_deref(),
            Some("\x1b[<2;1;1M")
        );
        assert_eq!(
            sgr_bytes(mode, press_at(PtyMouseButton::WheelDown, 1, 1)).as_deref(),
            Some("\x1b[<65;1;1M")
        );
        // Horizontal wheel notches are buttons 66/67, not a separate protocol.
        assert_eq!(
            sgr_bytes(mode, press_at(PtyMouseButton::WheelLeft, 1, 1)).as_deref(),
            Some("\x1b[<66;1;1M")
        );

        // Motion sets bit 5 on top of the button, and bare motion reports the
        // "no button" code 3 — so a drag with the left button is 32 and a
        // pointer move with nothing held is 35.
        assert_eq!(
            sgr_bytes(
                mode,
                PtyMouseReport {
                    action: PtyMouseAction::Drag(PtyMouseButton::Left),
                    ..press_at(PtyMouseButton::Left, 4, 9)
                }
            )
            .as_deref(),
            Some("\x1b[<32;4;9M")
        );
        assert_eq!(
            sgr_bytes(
                mode,
                PtyMouseReport {
                    action: PtyMouseAction::Move,
                    ..press_at(PtyMouseButton::Left, 4, 9)
                }
            )
            .as_deref(),
            Some("\x1b[<35;4;9M")
        );

        // Shift 4, Alt 8, Ctrl 16, added to the button field.
        assert_eq!(
            sgr_bytes(
                mode,
                PtyMouseReport {
                    shift: true,
                    alt: true,
                    ctrl: true,
                    ..press_at(PtyMouseButton::Left, 1, 1)
                }
            )
            .as_deref(),
            Some("\x1b[<28;1;1M")
        );
        // ...except in X10, which predates them entirely.
        assert_eq!(
            sgr_bytes(
                MouseProtocolMode::Press,
                PtyMouseReport {
                    ctrl: true,
                    ..press_at(PtyMouseButton::Left, 1, 1)
                }
            )
            .as_deref(),
            Some("\x1b[<0;1;1M")
        );
    }

    #[test]
    fn a_release_keeps_its_button_in_sgr_and_loses_it_in_the_legacy_encodings() {
        let release = PtyMouseReport {
            action: PtyMouseAction::Release(PtyMouseButton::Right),
            ..press_at(PtyMouseButton::Right, 3, 7)
        };

        // SGR says which button, and marks the release with a lowercase `m`.
        assert_eq!(
            sgr_bytes(MouseProtocolMode::PressRelease, release).as_deref(),
            Some("\x1b[<2;3;7m")
        );
        // The legacy encodings have neither, so a release is the anonymous
        // "button 3" report. A program told `2` here would believe the right
        // button is still down.
        assert_eq!(
            mouse_report_bytes(
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default,
                release
            ),
            Some(vec![0x1b, b'[', b'M', 3 + 32, 3 + 32, 7 + 32])
        );
        // Modifiers still ride along on the anonymous release.
        assert_eq!(
            mouse_report_bytes(
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default,
                PtyMouseReport {
                    ctrl: true,
                    ..release
                }
            ),
            Some(vec![0x1b, b'[', b'M', 3 + 16 + 32, 3 + 32, 7 + 32])
        );
    }

    #[test]
    fn each_mouse_mode_gets_only_the_events_it_asked_for() {
        let left = PtyMouseButton::Left;
        let press = press_at(left, 1, 1);
        let release = PtyMouseReport {
            action: PtyMouseAction::Release(left),
            ..press
        };
        let drag = PtyMouseReport {
            action: PtyMouseAction::Drag(left),
            ..press
        };
        let moved = PtyMouseReport {
            action: PtyMouseAction::Move,
            ..press
        };
        let wheel = press_at(PtyMouseButton::WheelUp, 1, 1);

        // A program that never enabled the mouse hears nothing at all.
        for report in [press, release, drag, moved, wheel] {
            assert_eq!(sgr_bytes(MouseProtocolMode::None, report), None);
        }
        // X10 (mode 9) is press-only: a release or a drag sent here is a
        // sequence the program never asked to parse.
        assert!(sgr_bytes(MouseProtocolMode::Press, press).is_some());
        assert!(sgr_bytes(MouseProtocolMode::Press, wheel).is_some());
        assert_eq!(sgr_bytes(MouseProtocolMode::Press, release), None);
        assert_eq!(sgr_bytes(MouseProtocolMode::Press, drag), None);
        assert_eq!(sgr_bytes(MouseProtocolMode::Press, moved), None);
        // VT200 (1000) adds releases but not motion.
        assert!(sgr_bytes(MouseProtocolMode::PressRelease, release).is_some());
        assert_eq!(sgr_bytes(MouseProtocolMode::PressRelease, drag), None);
        assert_eq!(sgr_bytes(MouseProtocolMode::PressRelease, moved), None);
        // Button-motion (1002) adds drags but still not free movement — this is
        // the mode most editors use, and reporting every pointer move to one
        // would be a stream of input it does not expect.
        assert!(sgr_bytes(MouseProtocolMode::ButtonMotion, drag).is_some());
        assert_eq!(sgr_bytes(MouseProtocolMode::ButtonMotion, moved), None);
        // Any-motion (1003) takes everything.
        assert!(sgr_bytes(MouseProtocolMode::AnyMotion, moved).is_some());

        // A wheel notch is a press in every mode and never has a release.
        assert_eq!(
            sgr_bytes(
                MouseProtocolMode::AnyMotion,
                PtyMouseReport {
                    action: PtyMouseAction::Release(PtyMouseButton::WheelUp),
                    ..wheel
                }
            ),
            None
        );
    }

    #[test]
    fn mouse_events_are_forwarded_to_a_mouse_reporting_program() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 24, cols: 80 });
        // Claude Code's startup: enter the alternate screen and request SGR
        // mouse reporting with drag tracking. After this the program owns the
        // pointer.
        runtime.process_terminal_output(terminal, b"\x1b[?1049h\x1b[?1002h\x1b[?1006h");
        assert!(runtime.terminal_reports_mouse(terminal));

        assert!(runtime.forward_mouse(terminal, press_at(PtyMouseButton::WheelUp, 12, 5)));
        assert_eq!(
            read_client_message(&mut server_stream, "reading forwarded wheel"),
            ClientMessage::Input {
                pane,
                lease: test_lease(),
                bytes: b"\x1b[<64;12;5M".to_vec(),
            }
        );

        assert!(runtime.forward_mouse(terminal, press_at(PtyMouseButton::Left, 3, 4)));
        assert_eq!(
            read_client_message(&mut server_stream, "reading forwarded press"),
            ClientMessage::Input {
                pane,
                lease: test_lease(),
                bytes: b"\x1b[<0;3;4M".to_vec(),
            }
        );

        assert!(runtime.forward_mouse(
            terminal,
            PtyMouseReport {
                action: PtyMouseAction::Release(PtyMouseButton::Left),
                ..press_at(PtyMouseButton::Left, 3, 4)
            }
        ));
        assert_eq!(
            read_client_message(&mut server_stream, "reading forwarded release"),
            ClientMessage::Input {
                pane,
                lease: test_lease(),
                bytes: b"\x1b[<0;3;4m".to_vec(),
            }
        );

        // The program asked for 1002, not 1003, so a bare pointer move is not
        // written at all — not even an empty message.
        assert!(!runtime.forward_mouse(
            terminal,
            PtyMouseReport {
                action: PtyMouseAction::Move,
                ..press_at(PtyMouseButton::Left, 3, 5)
            }
        ));
    }

    #[test]
    fn mouse_events_are_not_forwarded_when_the_program_ignores_the_mouse() {
        let mut runtime = PtyRuntime::new_offline();
        let terminal = PtyKey::Terminal(TerminalId(7));
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree");

        assert!(!runtime.terminal_reports_mouse(terminal));
        assert!(!runtime.forward_mouse(terminal, press_at(PtyMouseButton::WheelUp, 1, 1)));
    }

    #[test]
    fn a_forwarded_mouse_event_does_not_leave_the_scrollback_view() {
        // A synthetic write is not typing: it must not yank a reader who is
        // scrolled back down to the bottom the way a keystroke does.
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree\r\nfour");
        // Mouse reporting without the alternate screen: a program can grab the
        // pointer and still have real scrollback behind it.
        runtime.process_terminal_output(terminal, b"\x1b[?1000h\x1b[?1006h");
        assert!(runtime.scroll_up(terminal, 2).expect("scroll up"));

        assert!(runtime.forward_mouse(terminal, press_at(PtyMouseButton::Left, 1, 1)));
        assert_eq!(runtime.parser(terminal).unwrap().screen().scrollback(), 2);

        // Typing, by contrast, still returns to the bottom.
        assert!(runtime.send_input(terminal, b"x").expect("send input"));
        assert_eq!(runtime.parser(terminal).unwrap().screen().scrollback(), 0);
    }

    #[test]
    fn focus_reporting_is_tracked_and_only_sent_when_the_program_asked() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 24, cols: 80 });

        // Nothing is sent to a program that never enabled DECSET 1004.
        assert!(!runtime.terminal_reports_focus(terminal));
        assert!(!runtime.send_focus(terminal, true));

        // `process_terminal_output` does not run the responder; only real
        // daemon output does, which is what `feed_terminal_output` models.
        runtime.feed_terminal_output(terminal, b"\x1b[?1004h", true);
        assert!(runtime.terminal_reports_focus(terminal));

        assert!(runtime.send_focus(terminal, false));
        assert_eq!(
            read_client_message(&mut server_stream, "reading focus out"),
            ClientMessage::Input {
                pane,
                lease: test_lease(),
                bytes: b"\x1b[O".to_vec(),
            }
        );
        assert!(runtime.send_focus(terminal, true));
        assert_eq!(
            read_client_message(&mut server_stream, "reading focus in"),
            ClientMessage::Input {
                pane,
                lease: test_lease(),
                bytes: b"\x1b[I".to_vec(),
            }
        );

        // A program that turns the mode back off stops being told.
        runtime.feed_terminal_output(terminal, b"\x1b[?1004l", true);
        assert!(!runtime.terminal_reports_focus(terminal));
        assert!(!runtime.send_focus(terminal, true));
    }

    #[test]
    fn a_pane_title_is_read_from_osc_and_sanitized_before_it_is_shown() {
        let mut runtime = PtyRuntime::new_offline();
        let terminal = PtyKey::Terminal(TerminalId(7));
        runtime.ensure_parser(terminal, PtyDimensions { rows: 24, cols: 80 });

        // A pane nobody has titled has no title, rather than an empty one.
        assert_eq!(runtime.terminal_title(terminal), None);

        // OSC 0 sets icon name and title together; OSC 2 sets the title alone.
        // Both terminate with BEL or with ST.
        runtime.process_terminal_output(terminal, b"\x1b]0;~/projects/mult\x07");
        assert_eq!(
            runtime.terminal_title(terminal).as_deref(),
            Some("~/projects/mult")
        );
        runtime.process_terminal_output(terminal, b"\x1b]2;nvim: pty.rs\x1b\\");
        assert_eq!(
            runtime.terminal_title(terminal).as_deref(),
            Some("nvim: pty.rs")
        );

        // A title blanked by the program reads as absent, so a label falls back
        // instead of rendering nothing.
        runtime.process_terminal_output(terminal, b"\x1b]2;   \x07");
        assert_eq!(runtime.terminal_title(terminal), None);

        // `vt100` keeps whatever payload reaches it verbatim, so neither bound
        // holds by itself. A newline or a tab inside a title would break the
        // row the label is drawn in, so the controls are dropped.
        runtime.process_terminal_output(terminal, b"\x1b]2;one\ntwo\tthree\x07");
        assert_eq!(
            runtime.terminal_title(terminal).as_deref(),
            Some("onetwothree")
        );
        // An embedded escape sequence never gets that far: ESC begins ST, so
        // the parser ends the OSC there and what follows is an ordinary CSI
        // aimed at the screen. The title keeps only what preceded it.
        runtime.process_terminal_output(terminal, b"\x1b]2;title\x1b[31mred\x07");
        assert_eq!(runtime.terminal_title(terminal).as_deref(), Some("title"));

        // ...and an unbounded title is cut to a bound, not copied whole on
        // every frame.
        let huge = format!("\x1b]2;{}\x07", "t".repeat(4096));
        runtime.process_terminal_output(terminal, huge.as_bytes());
        assert_eq!(
            runtime
                .terminal_title(terminal)
                .map(|title| title.chars().count()),
            Some(MAX_PANE_TITLE_CHARS)
        );
    }

    #[test]
    fn only_a_focus_change_is_reported_not_the_state_every_tick() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 24, cols: 80 });
        runtime.feed_terminal_output(terminal, b"\x1b[?1004h", true);

        // The render loop hands the same answer over every tick; only the first
        // is a transition.
        runtime.set_focused_pane(Some(terminal));
        runtime.set_focused_pane(Some(terminal));
        runtime.set_focused_pane(Some(terminal));
        runtime.set_focused_pane(None);

        // Exactly one focus-in then one focus-out: a repeat would show up here
        // as a second `\x1b[I` ahead of the `\x1b[O`.
        for expected in [b"\x1b[I".to_vec(), b"\x1b[O".to_vec()] {
            assert_eq!(
                read_client_message(&mut server_stream, "reading focus report"),
                ClientMessage::Input {
                    pane,
                    lease: test_lease(),
                    bytes: expected,
                }
            );
        }
    }

    #[test]
    fn focus_reporting_is_recognised_beside_other_modes_and_not_confused_with_them() {
        fn feed(detector: &mut TerminalResponseDetector, bytes: &[u8]) {
            for byte in bytes {
                detector.advance(*byte);
            }
        }

        let mut detector = TerminalResponseDetector::default();
        // A neighbouring mode number must not set it...
        feed(&mut detector, b"\x1b[?1049h\x1b[?1000h\x1b[?10041h");
        assert!(!detector.focus_reporting);
        // ...nor may the non-private form, which is a different mode space.
        feed(&mut detector, b"\x1b[1004h");
        assert!(!detector.focus_reporting);
        // A single DECSET setting several modes at once still counts.
        feed(&mut detector, b"\x1b[?1004;1006h");
        assert!(detector.focus_reporting);
        // And the matching reset clears it, whichever position it is in.
        feed(&mut detector, b"\x1b[?1006;1004l");
        assert!(!detector.focus_reporting);
    }

    #[test]
    fn parser_scrolls_beyond_visible_screen_height() {
        let mut runtime = PtyRuntime::new_offline();
        let terminal = PtyKey::Terminal(TerminalId(7));
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");

        assert!(runtime.scroll_up(terminal, 4).expect("scroll up"));

        assert_eq!(runtime.parser(terminal).unwrap().screen().scrollback(), 4);
        assert_eq!(
            runtime.terminal_lines(terminal),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn pty_scroll_is_local_and_paste_sends_input_message() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree");

        assert!(runtime.scroll_up(terminal, 1).expect("scroll up"));
        assert!(runtime.scroll_down(terminal, 1).expect("scroll down"));
        assert!(!runtime
            .scroll_up(PtyKey::Terminal(TerminalId(99)), 1)
            .expect("missing"));
        assert!(runtime.send_paste(terminal, "one\ntwo").expect("paste"));

        let message = read_client_message(&mut server_stream, "reading pasted client input");
        assert_eq!(
            message,
            ClientMessage::Paste {
                pane,
                lease: test_lease(),
                bytes: b"one\ntwo".to_vec(),
            }
        );
    }

    #[test]
    fn pty_stop_keeps_local_attachment_when_send_fails() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let writer = Arc::new(Mutex::new(client_stream));
        let poison_writer = writer.clone();
        let _ = thread::spawn(move || {
            let _guard = poison_writer.lock().expect("lock writer");
            panic!("poison writer lock");
        })
        .join();
        let mut runtime = PtyRuntime::disconnected(unique_socket_path(), Vec::new());
        runtime.connection = Some(ServerConnection { writer, receiver });
        runtime.client_scope = Some(test_scope());
        runtime.server_instance = Some(test_server_instance());
        runtime.bind_pane(terminal, pane);
        let entry = runtime.pane_entry(terminal);
        entry.lease = Some(test_lease());
        entry.expected_output = Some(OutputSequence::ZERO);
        entry.identity = Some(test_wire_session_identity(terminal));

        let error = runtime.stop(terminal).expect_err("stop should fail");

        // A poisoned local writer lock is our own I/O failing, not the daemon
        // refusing anything, so it carries no `RejectCode`.
        assert!(matches!(error, PtyError::Io(_)));
        assert_eq!(error.code(), None);
        assert!(runtime.is_running(terminal));
        assert_eq!(runtime.key_for_pane(pane), Some(terminal));
    }

    /// F2: the per-key state used to be twelve parallel maps and every
    /// lifecycle operation had to remember which subset to touch. These two
    /// assert the two boundaries that mattered: retiring a key drops
    /// *everything* it owned, and losing an attachment drops only what the
    /// attachment owned.
    #[test]
    fn removing_a_terminal_drops_every_field_it_owned() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::ChatAgent(ChatId(5));
        let pane = pane_for_key(terminal);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 4, cols: 10 });
        runtime.process_terminal_output(terminal, b"output");
        runtime.pane_entry(terminal).identity = Some(test_wire_session_identity(terminal));
        runtime
            .register_agent_session(
                terminal,
                AgentSessionMetadata {
                    schema_version: mult_protocol::AGENT_STATUS_SCHEMA_VERSION,
                    chat_id: 5,
                    agent: mult_protocol::AgentKind::Pi,
                    generation: mult_protocol::AgentGeneration::from_bytes([7; 16]).unwrap(),
                },
            )
            .expect("register agent metadata");
        runtime.record_exit_status_for_test(
            terminal,
            PtyExit {
                code: 1,
                signal: None,
            },
        );
        runtime.record_foreground_process(
            terminal,
            ForegroundProcessInfo {
                root_pid: Some(1),
                foreground_pid: Some(2),
                command: Some("cargo test".to_string()),
            },
        );
        // Every field is populated, so nothing below can pass by never having
        // been set.
        assert!(runtime.panes.contains_key(&terminal));
        assert_eq!(runtime.terminal_last_command(terminal), Some("cargo test"));

        runtime.remove_terminal(terminal);

        assert!(!runtime.panes.contains_key(&terminal));
        assert_eq!(runtime.key_for_pane(pane), None);
        assert_eq!(runtime.attached_pane(terminal), None);
        assert_eq!(runtime.lease_of(pane), None);
        assert!(!runtime.is_running(terminal));
        assert!(runtime.parser(terminal).is_none());
        assert!(runtime.registered_session_identity(terminal).is_none());
        assert!(runtime.terminal_exit_status(terminal).is_none());
        assert!(runtime.terminal_last_command(terminal).is_none());
        assert!(runtime.terminal_output_is_blank(terminal));
    }

    #[test]
    fn clearing_an_attachment_keeps_everything_the_attachment_did_not_own() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(6));
        let pane = PaneId(6);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 4, cols: 10 });
        runtime.process_terminal_output(terminal, b"scrollback");
        runtime.pane_entry(terminal).identity = Some(test_wire_session_identity(terminal));

        assert_eq!(runtime.clear_attachment(pane), Some(terminal));

        // Gone: the binding and the lease state that only exists with it.
        assert_eq!(runtime.key_for_pane(pane), None);
        assert_eq!(runtime.attached_pane(terminal), None);
        assert_eq!(runtime.lease_of(pane), None);
        assert!(!runtime.is_running(terminal));
        // Kept: the model still shows this terminal, and a re-attach reuses it.
        assert!(runtime.parser(terminal).is_some());
        assert!(runtime.terminal_lines(terminal)[0].contains("scrollback"));
        assert!(runtime.registered_session_identity(terminal).is_some());
    }

    #[test]
    fn pane_exit_event_clears_local_attachment() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(9));
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);

        sender
            .send(ServerMessage::PaneExited {
                pane,
                lease: test_lease(),
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
                terminal,
                status: PtyExit {
                    code: 3,
                    signal: None,
                },
            }]
        );
        assert!(!runtime.is_running(terminal));
        assert_eq!(runtime.key_for_pane(pane), None);
        assert_eq!(
            runtime.terminal_exit_status(terminal),
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
        let terminal = PtyKey::Terminal(TerminalId(9));
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);

        assert!(runtime
            .send_input(terminal, b"cargo test")
            .expect("send command"));
        let _ = read_client_message(&mut server_stream, "reading command input");
        assert_eq!(runtime.terminal_last_command(terminal), None);

        assert!(runtime.send_input(terminal, b"\r").expect("send enter"));
        let _ = read_client_message(&mut server_stream, "reading enter input");

        assert_eq!(runtime.terminal_last_command(terminal), Some("cargo test"));
    }

    #[test]
    fn terminal_last_command_ignores_fullscreen_app_input() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(9));
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });

        assert!(runtime.send_input(terminal, b"nvim\r").expect("send nvim"));
        let _ = read_client_message(&mut server_stream, "reading nvim input");
        assert_eq!(runtime.terminal_last_command(terminal), Some("nvim"));

        runtime.process_terminal_output(terminal, b"\x1b[?1049h");
        assert!(runtime
            .send_input(terminal, b"asdasdq\r")
            .expect("send editor input"));
        let _ = read_client_message(&mut server_stream, "reading editor input");

        assert_eq!(runtime.terminal_last_command(terminal), Some("nvim"));
    }

    #[test]
    fn terminal_last_command_uses_foreground_process_not_child_input() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(9));
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);

        sender
            .send(ServerMessage::ForegroundProcess {
                pane,
                lease: test_lease(),
                process: ForegroundProcessInfo {
                    root_pid: Some(10),
                    foreground_pid: Some(20),
                    command: Some("python".to_string()),
                },
            })
            .expect("send foreground process");
        assert!(runtime.drain_events().is_empty());
        assert_eq!(runtime.terminal_last_command(terminal), Some("python"));

        assert!(runtime
            .send_input(terminal, b"print('typed text')\r")
            .expect("send child input"));
        let _ = read_client_message(&mut server_stream, "reading child input");
        assert_eq!(runtime.terminal_last_command(terminal), Some("python"));

        sender
            .send(ServerMessage::ForegroundProcess {
                pane,
                lease: test_lease(),
                process: ForegroundProcessInfo {
                    root_pid: Some(10),
                    foreground_pid: Some(10),
                    command: Some("bash".to_string()),
                },
            })
            .expect("send shell foreground process");
        assert!(runtime.drain_events().is_empty());
        assert!(runtime
            .send_input(terminal, b"cargo test\r")
            .expect("send shell input"));
        let _ = read_client_message(&mut server_stream, "reading shell input");
        assert_eq!(runtime.terminal_last_command(terminal), Some("cargo test"));
    }

    #[test]
    fn live_output_answers_primary_device_attributes_query() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(9));
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });

        sender
            .send(ServerMessage::PtyOutput {
                pane,
                lease: test_lease(),
                sequence: OutputSequence::ZERO,
                bytes: Arc::from(b"\x1b[c".to_vec()),
            })
            .expect("send terminal query");

        let events = runtime.drain_events();
        let message = read_client_message(&mut server_stream, "reading DA response");

        assert_eq!(
            events,
            vec![PtyEvent::Output {
                terminal,
                byte_count: 3,
            }]
        );
        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                lease: test_lease(),
                bytes: PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.to_vec(),
            }
        );
    }

    #[test]
    fn live_output_reports_cursor_after_a_batched_run() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(9));
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });

        // "abc" advances the cursor to column 3 and is fed as a batched run; the
        // trailing DSR cursor-position query must still report the cursor at the
        // query point (row 1, col 4 in 1-based terms), proving the batched feed
        // matches byte-by-byte behaviour.
        sender
            .send(ServerMessage::PtyOutput {
                pane,
                lease: test_lease(),
                sequence: OutputSequence::ZERO,
                bytes: Arc::from(b"abc\x1b[6n".to_vec()),
            })
            .expect("send output with embedded cursor query");

        let _ = runtime.drain_events();
        let message = read_client_message(&mut server_stream, "reading DSR response");

        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                lease: test_lease(),
                bytes: b"\x1b[1;4R".to_vec(),
            }
        );
    }

    #[test]
    fn a_system_line_cannot_carry_control_sequences_into_the_emulator() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(9));
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 4, cols: 40 });
        runtime.process_terminal_output(terminal, b"genuine pane output\r\n");

        // A daemon-supplied message that tries to erase the screen and forge a
        // line of its own.
        runtime.append_terminal_system_line(terminal, "\x1b[2J\x1b[Hexit 0\u{7}");

        let lines = runtime.terminal_lines(terminal);
        assert_eq!(
            lines[0], "genuine pane output",
            "the earlier output must survive: the escape never reached the parser"
        );
        assert_eq!(lines[1], "[mult] \u{fffd}[2J\u{fffd}[Hexit 0\u{fffd}");
    }

    #[test]
    fn a_flood_of_queries_produces_one_bounded_write() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(9));
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });

        // 8 KiB of cursor-position queries: once ~2048 separate blocking socket
        // writes from the render thread.
        let flood = b"\x1b[6n".repeat(2048);
        sender
            .send(ServerMessage::PtyOutput {
                pane,
                lease: test_lease(),
                sequence: OutputSequence::ZERO,
                bytes: Arc::from(flood),
            })
            .expect("send query flood");

        let _ = runtime.drain_events();
        let message = read_client_message(&mut server_stream, "reading coalesced response");

        let ClientMessage::Input { bytes, .. } = &message else {
            panic!("expected a single coalesced Input, got {message:?}");
        };
        assert_eq!(
            bytes,
            &b"\x1b[1;1R".to_vec(),
            "one cursor report answers the whole chunk"
        );
        assert!(bytes.len() <= MAX_TERMINAL_QUERY_RESPONSE_BYTES);

        // Nothing else was written: the flood cost exactly one message.
        server_stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set read timeout");
        let mut extra = [0_u8; 1];
        assert!(
            std::io::Read::read(&mut server_stream, &mut extra).is_err(),
            "the flood must not produce a second write"
        );
    }

    #[test]
    fn mixed_queries_are_coalesced_into_one_payload() {
        let mut parser = Parser::new(2, 8, TERMINAL_SCROLLBACK_LINES);
        let mut responder = TerminalResponseDetector::default();

        // Distinct queries are all answered, in order, in one payload.
        let payload = feed_parser_with_responder(&mut parser, &mut responder, b"\x1b[c\x1b[5n");

        let mut expected = PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.to_vec();
        expected.extend_from_slice(DEVICE_STATUS_OK_RESPONSE);
        assert_eq!(payload, expected);

        // The overall cap bounds even a flood of *answerable, distinct* queries.
        let payload = feed_parser_with_responder(
            &mut parser,
            &mut responder,
            &b"\x1b[c".repeat(MAX_TERMINAL_QUERY_RESPONSES_PER_CHUNK + 4),
        );
        assert_eq!(
            payload.len(),
            PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.len() * MAX_TERMINAL_QUERY_RESPONSES_PER_CHUNK
        );
    }

    /// Byte classes that reach the emulator from a real pane and that used to
    /// panic it at one row or one column: raw non-UTF-8, a double-width glyph,
    /// wrapping text, and the control bytes every shell emits.
    fn emulator_byte_classes() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("invalid-utf8", vec![0xff, 0xfe, 0xfd]),
            ("emoji", "😀😀".as_bytes().to_vec()),
            ("cjk", "漢字".as_bytes().to_vec()),
            ("ascii-overflowing-the-row", b"hello world".to_vec()),
            ("newlines", b"a\r\nb\r\nc\r\n".to_vec()),
            ("tabs", b"\tab\t\tcd".to_vec()),
            ("backspace", b"abc\x08\x08\x08".to_vec()),
            (
                "combining-marks",
                "e\u{301}\u{301}\u{301}".as_bytes().to_vec(),
            ),
            ("cursor-addressing", b"\x1b[2J\x1b[H\x1b[9;9Hx".to_vec()),
            ("wide-then-newline", "漢\r\n漢".as_bytes().to_vec()),
        ]
    }

    #[test]
    fn parser_dimensions_are_clamped_to_the_emulator_floor() {
        // Sizes a collapsed pane really reports. `fnug-vt100` panics with
        // "attempt to subtract with overflow" below 2×2 on ordinary output, so
        // no size may reach it unclamped (A13).
        let requested = [
            PtyDimensions { rows: 0, cols: 0 },
            PtyDimensions { rows: 1, cols: 1 },
            PtyDimensions { rows: 1, cols: 80 },
            PtyDimensions { rows: 24, cols: 1 },
            PtyDimensions { rows: 0, cols: 40 },
            PtyDimensions { rows: 40, cols: 0 },
            PtyDimensions {
                rows: MIN_SCREEN_ROWS,
                cols: MIN_SCREEN_COLS,
            },
        ];

        for size in requested {
            let clamped = size.clamped();
            assert!(
                clamped.rows >= MIN_SCREEN_ROWS && clamped.cols >= MIN_SCREEN_COLS,
                "{size:?} was not raised to the floor"
            );

            // Every public route into a parser, not just the clamp helper.
            for (index, build) in [0_u8, 1, 2].into_iter().enumerate() {
                let terminal = PtyKey::Terminal(TerminalId(200 + index as u64));
                let mut runtime = PtyRuntime::new_offline();
                match build {
                    0 => runtime.ensure_parser(terminal, size),
                    1 => runtime.reset_parser(terminal, size),
                    _ => runtime.resize(terminal, size).expect("offline resize"),
                }
                let parser = runtime.parser(terminal).expect("parser exists");
                assert_eq!(
                    parser.screen().size(),
                    (clamped.rows, clamped.cols),
                    "{size:?} reached the parser unclamped via route {build}"
                );

                for (name, bytes) in emulator_byte_classes() {
                    // A panic here is the A13 regression; it aborts the test
                    // process in debug and corrupts the grid in release.
                    runtime.process_terminal_output(terminal, &bytes);
                    assert!(
                        !runtime.terminal_lines(terminal).is_empty(),
                        "{name} at {size:?} produced no rows"
                    );
                }
            }
        }
    }

    #[test]
    fn narrowing_a_screen_holding_a_wide_character_does_not_panic() {
        // A14: `Row::resize` drops the second half of a double-width character
        // in the last column but leaves the first half flagged wide, and the
        // next print there unwraps `None` inside `fnug-vt100`. Dragging a pane
        // one column narrower with CJK or emoji on screen reaches it.
        for glyph in ["漢", "😀"] {
            for start_cols in [3_u16, 8, 21] {
                for end_cols in MIN_SCREEN_COLS..start_cols {
                    let terminal = PtyKey::Terminal(TerminalId(300));
                    let mut runtime = PtyRuntime::new_offline();
                    runtime.ensure_parser(
                        terminal,
                        PtyDimensions {
                            rows: 4,
                            cols: start_cols,
                        },
                    );
                    // Straddle the future last column: the glyph's first half
                    // lands on it and its second half is what the resize drops.
                    let padding = " ".repeat(usize::from(end_cols) - 1);
                    runtime
                        .process_terminal_output(terminal, format!("{padding}{glyph}").as_bytes());

                    runtime
                        .resize(
                            terminal,
                            PtyDimensions {
                                rows: 4,
                                cols: end_cols,
                            },
                        )
                        .expect("offline resize");
                    // The print that used to panic: it lands exactly on the
                    // half-glyph the truncation left behind.
                    runtime.process_terminal_output(
                        terminal,
                        format!("\x1b[1;{end_cols}Hx").as_bytes(),
                    );

                    let lines = runtime.terminal_lines(terminal);
                    assert!(
                        lines.first().is_some_and(|line| line.ends_with('x')),
                        "{glyph} {start_cols}->{end_cols} lost the print that follows the resize"
                    );
                    assert!(
                        !lines.first().is_some_and(|line| line.contains(glyph)),
                        "{glyph} {start_cols}->{end_cols} kept a character the resize cut in half"
                    );
                }
            }
        }
    }

    #[test]
    fn narrowing_keeps_rows_that_do_not_end_in_a_split_wide_character() {
        // The A14 repair must not erase anything the resize was not already
        // going to destroy: only the split glyph goes.
        let terminal = PtyKey::Terminal(TerminalId(301));
        let mut runtime = PtyRuntime::new_offline();
        runtime.ensure_parser(terminal, PtyDimensions { rows: 3, cols: 10 });
        runtime.process_terminal_output(terminal, "abcdefgh\r\nij漢klmn".as_bytes());

        runtime
            .resize(terminal, PtyDimensions { rows: 3, cols: 8 })
            .expect("offline resize");

        let lines = runtime.terminal_lines(terminal);
        assert_eq!(lines.first().map(String::as_str), Some("abcdefgh"));
        // The wide glyph sits at columns 2-3, well inside the new width, so it
        // survives untouched.
        assert_eq!(lines.get(1).map(String::as_str), Some("ij漢klmn"));
    }

    /// xorshift64*, mirroring `crates/protocol/tests/framing.rs`: the generated
    /// streams below must reproduce from their printed seed alone, so no
    /// `proptest` and no wall-clock or thread-local entropy.
    struct Rng(u64);

    impl Rng {
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

    /// The unbatched reference for [`feed_parser_with_responder`]: one
    /// `Parser::process` call per byte, with the responder observing the screen
    /// after every single one. The batched implementation documents itself as
    /// "behaviourally identical to feeding every byte individually"; this is
    /// what that sentence is measured against.
    fn feed_parser_byte_at_a_time(
        parser: &mut Parser,
        responder: &mut TerminalResponseDetector,
        bytes: &[u8],
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        let mut answered = 0_usize;
        let mut cursor_reported = false;
        for byte in bytes {
            parser.process(std::slice::from_ref(byte));
            let Some(query) = responder.advance(*byte) else {
                continue;
            };
            let response = query.resolve(parser.screen());
            let repeated_cursor =
                cursor_reported && response.kind == TerminalQueryKind::CursorPosition;
            let over_budget = answered >= MAX_TERMINAL_QUERY_RESPONSES_PER_CHUNK
                || payload.len() + response.bytes.len() > MAX_TERMINAL_QUERY_RESPONSE_BYTES;
            if !repeated_cursor && !over_budget {
                cursor_reported |= response.kind == TerminalQueryKind::CursorPosition;
                answered += 1;
                payload.extend_from_slice(&response.bytes);
            }
        }
        payload
    }

    /// Everything an observer of the two feeds can tell apart: what is on the
    /// screen, where the cursor is, how deep the scrollback went, and the bytes
    /// sent back to the pane.
    #[derive(Debug, PartialEq, Eq)]
    struct FeedObservation {
        contents: String,
        cursor: (u16, u16),
        scrollback_len: usize,
        payloads: Vec<Vec<u8>>,
    }

    fn observe_feed(
        rows: u16,
        cols: u16,
        chunks: &[&[u8]],
        feed: fn(&mut Parser, &mut TerminalResponseDetector, &[u8]) -> Vec<u8>,
    ) -> FeedObservation {
        let mut parser = Parser::new(rows, cols, TERMINAL_SCROLLBACK_LINES);
        let mut responder = TerminalResponseDetector::default();
        let payloads = chunks
            .iter()
            .map(|chunk| feed(&mut parser, &mut responder, chunk))
            .collect();
        let screen = parser.screen();
        FeedObservation {
            contents: screen.contents(),
            cursor: screen.cursor_position(),
            scrollback_len: screen.scrollback_len(),
            payloads,
        }
    }

    /// The hand-written half of the corpus: every shape where batching a run of
    /// bytes could plausibly diverge from feeding them one at a time.
    fn batching_corpus() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("plain-text", b"hello world".to_vec()),
            ("truncated-csi", b"abc\x1b[12".to_vec()),
            ("esc-at-end", b"abc\x1b".to_vec()),
            ("bare-esc-run", b"\x1b\x1b\x1b".to_vec()),
            ("oversized-csi", {
                let mut bytes = b"\x1b[".to_vec();
                bytes.extend(std::iter::repeat_n(
                    b'1',
                    TERMINAL_MAX_CSI_SEQUENCE_BYTES + 32,
                ));
                bytes.extend_from_slice(b"m tail");
                bytes
            }),
            ("cpr-mid-stream", b"ab\x1b[6ncd\x1b[6nef".to_vec()),
            ("cpr-after-move", b"\x1b[2;3H\x1b[6nx".to_vec()),
            ("private-cpr", b"\x1b[?6n\x1b[?6n".to_vec()),
            ("device-attributes", b"x\x1b[c\x1b[0cy".to_vec()),
            ("device-status", b"\x1b[5n\x1b[7n".to_vec()),
            ("decid", b"a\x1bZb".to_vec()),
            ("osc-title", b"\x1b]0;title\x07after".to_vec()),
            ("osc-st-terminated", b"\x1b]0;title\x1b\\after".to_vec()),
            ("dcs", b"\x1bP1$r0m\x1b\\rest".to_vec()),
            ("charset-designator", b"\x1b(B\x1b)0text".to_vec()),
            ("mixed-utf8", "héllo 漢字 😀 done".as_bytes().to_vec()),
            ("split-utf8-tail", {
                let mut bytes = "漢".as_bytes().to_vec();
                bytes.truncate(2);
                bytes
            }),
            ("invalid-utf8", vec![0xff, 0xfe, b'a', 0x80, b'b']),
            (
                "wrap-and-scroll",
                b"aaaaaaaaaaaa\r\nbbbbbbbbbbbb\r\ncccc".to_vec(),
            ),
            ("erase-and-move", b"xyz\x1b[2J\x1b[H\x1b[3;4Hq".to_vec()),
            ("scroll-region", b"\x1b[1;2r\r\n\r\n\r\nend".to_vec()),
            ("alternate-screen", b"a\x1b[?1049hb\x1b[?1049lc".to_vec()),
            ("query-flood", b"\x1b[6n".repeat(64)),
            ("csi-flood", b"\x1b[c".repeat(64)),
            (
                "control-bytes",
                vec![0x00, 0x07, 0x08, 0x09, 0x0a, 0x0d, 0x7f],
            ),
            ("c1-controls", vec![0x9b, b'6', b'n', 0x84, 0x85]),
        ]
    }

    /// Deterministic streams biased towards escape sequences, so the generated
    /// half of the corpus exercises the batched path rather than long printable
    /// runs.
    fn generated_stream(seed: u64, len: usize) -> Vec<u8> {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let fragments: [&[u8]; 14] = [
            b"\x1b[6n",
            b"\x1b[?6n",
            b"\x1b[c",
            b"\x1b[5n",
            b"\x1bZ",
            b"\x1b[2J",
            b"\x1b[H",
            b"\x1b[1;1H",
            b"\x1b]0;t\x07",
            b"\x1b(B",
            b"\x1b",
            b"\r\n",
            "漢".as_bytes(),
            "😀".as_bytes(),
        ];
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            match rng.below(4) {
                0 => bytes.push(0x20_u8.wrapping_add((rng.below(95)) as u8)),
                1 => bytes.push((rng.next_u64() >> 33) as u8),
                _ => bytes.extend_from_slice(fragments[rng.below(fragments.len())]),
            }
        }
        bytes.truncate(len);
        bytes
    }

    fn chunked(bytes: &[u8], chunk: usize) -> Vec<&[u8]> {
        bytes.chunks(chunk.max(1)).collect()
    }

    #[test]
    fn batched_feed_matches_byte_at_a_time_feed_on_adversarial_input() {
        // 1×1 is deliberately absent: the emulator cannot be driven there at
        // all (see `parser_dimensions_are_clamped_to_the_emulator_floor`), and
        // the equivalence claim is about batching, not about dimensions.
        for (rows, cols) in [(2_u16, 2_u16), (2, 8), (4, 10), (24, 80)] {
            for (name, bytes) in batching_corpus() {
                for chunk in [1_usize, 2, 3, 5, 7, 16, 64, usize::MAX] {
                    let chunks = chunked(&bytes, chunk);
                    let batched = observe_feed(rows, cols, &chunks, feed_parser_with_responder);
                    let reference = observe_feed(rows, cols, &chunks, feed_parser_byte_at_a_time);
                    assert_eq!(
                        batched, reference,
                        "case {name} diverged at {rows}x{cols} with chunk size {chunk}"
                    );
                }
            }
        }
    }

    #[test]
    fn batched_feed_matches_byte_at_a_time_feed_on_generated_streams() {
        for seed in 1..=64_u64 {
            let bytes = generated_stream(seed, 512);
            for chunk in [1_usize, 3, 8, 61, 512] {
                let chunks = chunked(&bytes, chunk);
                let batched = observe_feed(6, 20, &chunks, feed_parser_with_responder);
                let reference = observe_feed(6, 20, &chunks, feed_parser_byte_at_a_time);
                assert_eq!(
                    batched, reference,
                    "seed {seed} diverged with chunk size {chunk}"
                );
            }
        }
    }

    #[test]
    fn output_event_feeds_matching_parser() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(9));
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });

        sender
            .send(ServerMessage::PtyOutput {
                pane,
                lease: test_lease(),
                sequence: OutputSequence::ZERO,
                bytes: Arc::from(b"hello".to_vec()),
            })
            .expect("send output event");

        let events = runtime.drain_events();

        assert_eq!(
            events,
            vec![PtyEvent::Output {
                terminal,
                byte_count: 5,
            }]
        );
        assert_eq!(runtime.terminal_lines(terminal)[0], "hello");
    }

    #[test]
    fn adjacent_output_events_from_one_pane_are_coalesced() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(9));
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 16 });

        for (sequence, bytes) in [(0, b"hel".as_slice()), (3, b"lo".as_slice())] {
            sender
                .send(ServerMessage::PtyOutput {
                    pane,
                    lease: test_lease(),
                    sequence: OutputSequence::new(sequence),
                    bytes: Arc::from(bytes),
                })
                .expect("send output event");
        }

        let events = runtime.drain_events();

        // Two chunks, one notification: both are already in the parser, so the
        // render loop only needs to know that output arrived and how much.
        assert_eq!(
            events,
            vec![PtyEvent::Output {
                terminal,
                byte_count: 5,
            }]
        );
        assert_eq!(runtime.terminal_lines(terminal)[0], "hello");
    }

    #[test]
    fn lost_connection_retains_terminals_when_reconnect_fails() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(10));
        let pane = PaneId(10);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        drop(sender);

        let events = runtime.drain_events();

        // Unknown transport loss is not proof that the daemon lost the pane.
        // Keep the lease/mapping until a correlated attach reconciliation.
        assert!(runtime.connection.is_none());
        assert!(runtime.is_running(terminal));
        assert_eq!(runtime.key_for_pane(pane), Some(terminal));
        assert!(runtime.parser(terminal).is_some());
        // Losing the socket is a property of the connection, not of any one
        // pane, so it is reported without a pane rather than being blamed on
        // an arbitrary attached terminal or on `Terminal(0)` (E2/B8).
        assert!(events.iter().any(|event| matches!(
            event,
            PtyEvent::ConnectionError { message }
                if message.contains("retained pending reconciliation")
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PtyEvent::Error { .. })),
            "transport loss must not be attributed to a pane: {events:?}"
        );
    }

    #[test]
    fn a_connection_wide_server_error_is_not_attributed_to_a_pane() {
        // The protocol reserves `ServerMessage::Error` for the connection and
        // `LeaseRejected` for one pane, so `Error` must never be pinned to a
        // pane — neither an arbitrary attached one nor `Terminal(0)`, which
        // cannot exist because terminal ids start at 1 (B8).
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(11));
        let pane = PaneId(11);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        sender
            .send(ServerMessage::Error {
                code: RejectCode::TooManyPendingRequests,
                message: "daemon is at capacity".to_string(),
            })
            .expect("queue a connection-wide error");

        let events = runtime.drain_events();

        assert!(
            events.contains(&PtyEvent::ConnectionError {
                // The code leads: the status surface names the machine-readable
                // reason, and the daemon's prose only elaborates on it (F8).
                message: "too many pending requests: daemon is at capacity".to_string(),
            }),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PtyEvent::Error { .. })),
            "{events:?}"
        );
        // ...and the attachment it did not concern is untouched.
        assert_eq!(runtime.key_for_pane(pane), Some(terminal));
    }

    #[test]
    fn a_daemon_that_cannot_be_reached_reports_without_a_pane() {
        // The startup path is the one that had nothing attached at all, so its
        // diagnostic used to be queued against `Terminal(0)` and was therefore
        // never rendered anywhere: a user with a missing or version-mismatched
        // `mult-server` saw an inert UI (E2).
        let mut runtime = PtyRuntime::with_socket_path(
            PathBuf::from("/nonexistent/mult-e2-unreachable-socket/mult.sock"),
            SpawnPolicy::Autospawn,
        );

        let events = runtime.drain_events();

        assert!(
            events.iter().any(|event| matches!(
                event,
                PtyEvent::ConnectionError { message }
                    if message.contains("failed to connect to mult-server")
            )),
            "{events:?}"
        );
    }

    #[test]
    fn reattach_terminals_reattaches_known_sessions_with_parser_dimensions() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(5));
        let pane = pane_for_key(terminal);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 6, cols: 20 });
        let server = thread::spawn(move || {
            let message = read_client_message(&mut server_stream, "reading reattach");
            let ClientMessage::Attach {
                request_id,
                session,
                rows,
                cols,
                ..
            } = message
            else {
                panic!("expected Attach");
            };
            assert_eq!((session, rows, cols), (session_for_key(terminal), 6, 20));
            sender
                .send(ServerMessage::AttachResult {
                    request_id,
                    outcome: AttachOutcome::Attached {
                        session,
                        pane: mult_protocol::PaneInfo {
                            id: pane,
                            title: "reattached".to_string(),
                            rows,
                            cols,
                        },
                        lease: test_lease(),
                    },
                })
                .unwrap();
            sender
                .send(ServerMessage::ReplayBegin {
                    request_id,
                    pane,
                    lease: test_lease(),
                    first_sequence: OutputSequence::ZERO,
                    watermark: OutputSequence::ZERO,
                    omitted_prefix_bytes: 0,
                })
                .unwrap();
            sender
                .send(ServerMessage::ReplayEnd {
                    request_id,
                    pane,
                    lease: test_lease(),
                    watermark: OutputSequence::ZERO,
                })
                .unwrap();
        });

        runtime.enqueue_reattachments();
        runtime.service_reattachments();
        assert!(runtime.pending_reattach.is_empty());
        assert!(!runtime.has_pending_work());
        server.join().unwrap();
    }

    #[test]
    fn reattach_skips_the_terminal_currently_starting() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(5));
        let pane = pane_for_key(terminal);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 6, cols: 20 });
        runtime.starting = Some(terminal);

        runtime.enqueue_reattachments();
        runtime.service_reattachments();
        assert!(runtime.pending_reattach.is_empty());

        // The terminal being started must not be re-attached, so nothing is sent.
        server_stream
            .set_nonblocking(true)
            .expect("set nonblocking");
        assert!(read_message::<ClientMessage>(&mut server_stream).is_err());
    }

    #[test]
    fn request_allocator_is_bounded_and_never_wraps() {
        let mut runtime = PtyRuntime::new_offline();
        runtime.next_request_id = Some(RequestId::MAX);

        let last = runtime
            .allocate_request()
            .expect("allocate final request ID");
        assert_eq!(last, RequestId::MAX);
        runtime.finish_request(last);
        let error = runtime
            .allocate_request()
            .expect_err("request IDs must not wrap");
        assert!(error.to_string().contains("exhausted"));
    }

    #[test]
    fn correlated_results_are_deferred_across_operation_types_and_output() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(22));
        let pane = PaneId(22);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 20 });
        let create_id = test_request_id(1);
        let attach_id = test_request_id(2);
        let stop_id = test_request_id(3);
        sender
            .send(ServerMessage::StopResult {
                request_id: stop_id,
                outcome: StopOutcome::AlreadyAbsent,
            })
            .unwrap();
        sender
            .send(ServerMessage::PtyOutput {
                pane,
                lease: test_lease(),
                sequence: OutputSequence::ZERO,
                bytes: Arc::from(b"pane-b-output".to_vec()),
            })
            .unwrap();
        sender
            .send(ServerMessage::AttachResult {
                request_id: attach_id,
                outcome: AttachOutcome::Error(AttachError::RetryExpired),
            })
            .unwrap();
        sender
            .send(ServerMessage::CreateResult {
                request_id: create_id,
                outcome: CreateOutcome::Error(CreateError::RetryExpired),
            })
            .unwrap();

        let deadline = Instant::now() + TEST_IO_TIMEOUT;
        loop {
            let message = runtime.receive_for_request(create_id, deadline).unwrap();
            if message_request_id(&message) == Some(create_id) {
                break;
            }
            runtime.route_during_request(message);
        }
        assert_eq!(
            message_request_id(&runtime.receive_for_request(attach_id, deadline).unwrap()),
            Some(attach_id)
        );
        assert_eq!(
            message_request_id(&runtime.receive_for_request(stop_id, deadline).unwrap()),
            Some(stop_id)
        );
        assert!(runtime.terminal_lines(terminal)[0].contains("pane-b-output"));
        assert!(runtime.pending_events.iter().any(|event| matches!(
            event,
            PtyEvent::Output {
                terminal: event_terminal,
                byte_count: 13,
            } if *event_terminal == terminal
        )));
    }

    #[test]
    fn pane_a_attach_error_does_not_abort_pane_b_replay() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal_b = PtyKey::Terminal(TerminalId(32));
        let pane_b = PaneId(32);
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        runtime.bind_pane(terminal_b, pane_b);
        runtime.ensure_parser(terminal_b, PtyDimensions { rows: 2, cols: 20 });
        let pane_a_request = test_request_id(1);
        let pane_b_request = test_request_id(2);
        sender
            .send(ServerMessage::AttachResult {
                request_id: pane_a_request,
                outcome: AttachOutcome::Error(AttachError::SessionNotFound {
                    session: SessionId(31),
                }),
            })
            .unwrap();
        sender
            .send(ServerMessage::AttachResult {
                request_id: pane_b_request,
                outcome: AttachOutcome::Attached {
                    session: SessionId(32),
                    pane: mult_protocol::PaneInfo {
                        id: pane_b,
                        title: "pane b".to_string(),
                        rows: 2,
                        cols: 20,
                    },
                    lease: test_lease(),
                },
            })
            .unwrap();
        sender
            .send(ServerMessage::ReplayBegin {
                request_id: pane_b_request,
                pane: pane_b,
                lease: test_lease(),
                first_sequence: OutputSequence::ZERO,
                watermark: OutputSequence::new(1),
                omitted_prefix_bytes: 0,
            })
            .unwrap();
        sender
            .send(ServerMessage::ReplayChunk {
                request_id: pane_b_request,
                pane: pane_b,
                lease: test_lease(),
                sequence: OutputSequence::ZERO,
                bytes: Arc::from(b"b".to_vec()),
            })
            .unwrap();
        sender
            .send(ServerMessage::ReplayEnd {
                request_id: pane_b_request,
                pane: pane_b,
                lease: test_lease(),
                watermark: OutputSequence::new(1),
            })
            .unwrap();

        runtime
            .perform_attach(
                terminal_b,
                SessionId(32),
                PtyDimensions { rows: 2, cols: 20 },
                pane_b_request,
                ClientMessage::Attach {
                    request_id: pane_b_request,
                    identity: test_wire_session_identity(terminal_b),
                    session: SessionId(32),
                    rows: 2,
                    cols: 20,
                },
            )
            .expect("pane B attaches despite pane A failure");
        assert!(matches!(
            read_client_message(&mut server_stream, "pane B Attach"),
            ClientMessage::Attach {
                request_id,
                session: SessionId(32),
                ..
            } if request_id == pane_b_request
        ));
        assert!(runtime.is_running(terminal_b));
        assert_eq!(
            message_request_id(
                &runtime
                    .receive_for_request(pane_a_request, Instant::now() + TEST_IO_TIMEOUT)
                    .unwrap()
            ),
            Some(pane_a_request)
        );
    }

    #[test]
    fn takeover_clears_only_the_displaced_pane_lease() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("socket pair");
        let (_sender, receiver) = mpsc::channel();
        let first_terminal = PtyKey::Terminal(TerminalId(1));
        let first_pane = PaneId(1);
        let mut runtime = test_runtime(client_stream, receiver, first_terminal, first_pane);
        let second_terminal = PtyKey::Terminal(TerminalId(2));
        let second_pane = PaneId(2);
        let second_lease = test_lease().checked_next().expect("second lease");
        runtime.bind_pane(second_terminal, second_pane);
        let second_entry = runtime.pane_entry(second_terminal);
        second_entry.lease = Some(second_lease);
        second_entry.expected_output = Some(OutputSequence::ZERO);
        let mut events = Vec::new();

        runtime.handle_server_message(
            ServerMessage::TakenOver {
                pane: first_pane,
                lease: test_lease(),
            },
            &mut events,
        );

        assert!(!runtime.is_running(first_terminal));
        assert!(runtime.is_running(second_terminal));
        assert_eq!(runtime.lease_of(second_pane), Some(second_lease));
        assert_eq!(
            events,
            vec![PtyEvent::TakenOver {
                terminal: first_terminal
            }]
        );
    }

    #[test]
    fn live_output_gap_marks_attachment_unreconciled() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(33));
        let pane = PaneId(33);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        let mut events = Vec::new();

        runtime.handle_server_message(
            ServerMessage::PtyOutput {
                pane,
                lease: test_lease(),
                sequence: OutputSequence::new(1),
                bytes: Arc::from(b"gap".to_vec()),
            },
            &mut events,
        );

        assert!(!runtime.is_running(terminal));
        assert_eq!(runtime.attached_pane(terminal), Some(pane));
        assert!(events.iter().any(|event| matches!(
            event,
            PtyEvent::Error { message, .. } if message.contains("fresh attach replay")
        )));
    }

    #[test]
    fn failed_input_delivery_is_uncertain_and_is_not_replayed() {
        let (client_stream, server_stream) = UnixStream::pair().expect("socket pair");
        server_stream
            .shutdown(std::net::Shutdown::Both)
            .expect("shut down peer");
        let (_sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(3));
        let pane = PaneId(3);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);

        let error = runtime
            .send_input(terminal, b"must-not-replay")
            .expect_err("closed peer makes delivery uncertain");

        assert!(error
            .delivery()
            .is_some_and(|error| error.operation == PtyDeliveryOperation::Input));
        assert!(runtime.connection.is_none());
        assert!(runtime.is_running(terminal));
        assert_eq!(runtime.lease_of(pane), Some(test_lease()));
    }

    #[test]
    fn possibly_delivered_input_and_paste_are_reported_uncertain_once() {
        #[derive(Default)]
        struct FailAfterFrame {
            bytes: Vec<u8>,
            flushes: usize,
        }

        impl Write for FailAfterFrame {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "disconnect after possible delivery",
                ))
            }
        }

        let pane = PaneId(44);
        for (operation, message) in [
            (
                PtyDeliveryOperation::Input,
                ClientMessage::Input {
                    pane,
                    lease: test_lease(),
                    bytes: b"input-once".to_vec(),
                },
            ),
            (
                PtyDeliveryOperation::Paste,
                ClientMessage::Paste {
                    pane,
                    lease: test_lease(),
                    bytes: b"paste-once".to_vec(),
                },
            ),
        ] {
            let mut writer = FailAfterFrame::default();
            let error = write_non_replayable_frame(&mut writer, &message, operation, pane)
                .expect_err("flush failure after a complete frame is uncertain");
            assert!(error
                .delivery()
                .is_some_and(|delivery| delivery.operation == operation));
            assert_eq!(writer.flushes, 1, "the frame is never replayed");
            assert_eq!(
                read_message::<ClientMessage>(&mut writer.bytes.as_slice()).unwrap(),
                message,
                "the one possibly delivered frame is complete"
            );
        }
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

    #[test]
    fn autospawned_server_environment_is_an_allow_list() {
        for allowed in [
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
            assert!(
                server_env_is_allowed(OsStr::new(allowed)),
                "{allowed} is needed by the daemon"
            );
        }

        // The whole point: secrets in the first client's shell must not become
        // the long-lived daemon's environment, and thus every later pane's.
        for denied in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "GITHUB_TOKEN",
            "PATH_EXTRA",
            "MULTIPASS",
        ] {
            assert!(
                !server_env_is_allowed(OsStr::new(denied)),
                "{denied} must not reach the daemon"
            );
        }
    }

    #[test]
    fn only_an_unwritable_daemon_binary_is_trusted() {
        use std::os::unix::fs::PermissionsExt;

        let directory = unique_socket_path().with_extension("bin");
        fs::create_dir_all(&directory).expect("create binary directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
            .expect("restrict directory");
        let binary = directory.join("mult-server");
        fs::write(&binary, b"#!/bin/sh\n").expect("write binary");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("mark executable");

        assert!(is_trusted_executable(&binary));
        assert!(!is_trusted_executable(&directory));
        assert!(!is_trusted_executable(&directory.join("absent")));

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o777)).expect("widen binary");
        assert!(
            !is_trusted_executable(&binary),
            "a world-writable daemon binary must never be executed"
        );

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("restore binary");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o777))
            .expect("widen directory");
        assert!(
            !is_trusted_executable(&binary),
            "a world-writable directory lets the binary be replaced"
        );

        fs::remove_dir_all(&directory).expect("remove binary directory");
    }

    /// N4: a stalled daemon must cost one attach round trip per drain, not one
    /// per terminal. Two terminals are queued and the daemon answers neither;
    /// exactly one `Attach` may leave the client before the budget stops it.
    #[test]
    fn a_stalled_reattach_starts_only_one_round_trip_per_drain() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let first = PtyKey::Terminal(TerminalId(21));
        let second = PtyKey::Terminal(TerminalId(22));
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        for terminal in [first, second] {
            runtime.bind_pane(terminal, pane_for_key(terminal));
            runtime.ensure_parser(terminal, PtyDimensions { rows: 4, cols: 10 });
        }

        runtime.enqueue_reattachments();
        // Bounded by `ATTACH_ACK_TIMEOUT`: the daemon never answers, so this
        // returns after one unanswered round trip rather than two.
        runtime.service_reattachments();

        server_stream
            .set_read_timeout(Some(TEST_IO_TIMEOUT))
            .expect("set read timeout");
        let sent = read_client_message(&mut server_stream, "reading the first reattach");
        assert!(matches!(sent, ClientMessage::Attach { .. }));
        server_stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set short read timeout");
        assert!(
            read_message::<ClientMessage>(&mut server_stream).is_err(),
            "the second terminal must wait for a later drain"
        );
        assert_eq!(runtime.pending_reattach.len(), 2);
        assert!(runtime.has_pending_work());
    }

    /// N4: the flip side — once the daemon answers, every queued terminal is
    /// re-attached, and well within a single drain.
    #[test]
    fn queued_reattachments_all_complete_against_a_healthy_daemon() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminals = [
            PtyKey::Terminal(TerminalId(31)),
            PtyKey::Terminal(TerminalId(32)),
            PtyKey::Terminal(TerminalId(33)),
        ];
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        for terminal in terminals {
            runtime.bind_pane(terminal, pane_for_key(terminal));
            runtime.ensure_parser(terminal, PtyDimensions { rows: 4, cols: 10 });
        }

        let server = thread::spawn(move || {
            server_stream
                .set_read_timeout(Some(TEST_IO_TIMEOUT))
                .expect("set read timeout");
            for _ in 0..terminals.len() {
                let ClientMessage::Attach {
                    request_id,
                    session,
                    rows,
                    cols,
                    ..
                } = read_client_message(&mut server_stream, "reading a queued reattach")
                else {
                    panic!("expected Attach");
                };
                let pane = PaneId(session.0);
                sender
                    .send(ServerMessage::AttachResult {
                        request_id,
                        outcome: AttachOutcome::Attached {
                            session,
                            pane: mult_protocol::PaneInfo {
                                id: pane,
                                title: "reattached".to_string(),
                                rows,
                                cols,
                            },
                            lease: test_lease(),
                        },
                    })
                    .expect("send attach result");
                sender
                    .send(ServerMessage::ReplayBegin {
                        request_id,
                        pane,
                        lease: test_lease(),
                        first_sequence: OutputSequence::ZERO,
                        watermark: OutputSequence::ZERO,
                        omitted_prefix_bytes: 0,
                    })
                    .expect("send replay begin");
                sender
                    .send(ServerMessage::ReplayEnd {
                        request_id,
                        pane,
                        lease: test_lease(),
                        watermark: OutputSequence::ZERO,
                    })
                    .expect("send replay end");
            }
        });

        runtime.enqueue_reattachments();
        runtime.service_reattachments();
        server.join().expect("reattach server thread");

        assert!(runtime.pending_reattach.is_empty());
        assert!(!runtime.has_pending_work());
        for terminal in terminals {
            assert!(
                runtime.is_running(terminal),
                "{terminal:?} was not reattached"
            );
        }
    }

    /// B6: `ensure_connected` never performs the connect/hello round trip
    /// itself. It reports `NotConnected` at once and the connector thread's
    /// result is collected by a later drain.
    #[test]
    fn connection_establishment_happens_off_the_calling_thread() {
        let socket_path = unique_socket_path();
        let listener = UnixListener::bind(&socket_path).expect("bind test daemon socket");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            stream
                .set_read_timeout(Some(TEST_IO_TIMEOUT))
                .expect("set read timeout");
            let hello = read_message::<ClientMessage>(&mut stream).expect("read client hello");
            assert!(matches!(hello, ClientMessage::Hello { .. }));
            write_message(
                &mut stream,
                &ServerMessage::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    server_instance: test_server_instance(),
                    client_scope: test_scope(),
                    resumed: false,
                },
            )
            .expect("write server hello");
            // Hold the accepted socket open so the client's reader thread stays
            // alive for the assertion below.
            thread::sleep(Duration::from_millis(50));
        });

        let mut runtime = PtyRuntime::disconnected(socket_path.clone(), Vec::new());
        let error = runtime
            .ensure_connected()
            .expect_err("a disconnected runtime must not block on connect");
        assert!(matches!(error, PtyError::Offline(_)));

        let deadline = Instant::now() + TEST_IO_TIMEOUT;
        while runtime.connection.is_none() && Instant::now() < deadline {
            runtime.drain_events();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            runtime.connection.is_some(),
            "the background connector's result must be collected by a drain"
        );

        server.join().expect("hello server thread");
        let _ = fs::remove_file(&socket_path);
    }

    /// B5: dropping a connection must shut the socket down, or the reader
    /// thread stays parked on its own descriptor forever.
    #[test]
    fn dropping_a_connection_releases_the_parked_reader_thread() {
        use std::io::Read;

        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        // Stands in for the reader thread's own `dup` of the socket: it keeps
        // the descriptor open after the runtime drops its writer handle.
        let mut parked = client_stream.try_clone().expect("clone client stream");
        let (exited, reader_exited) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut byte = [0_u8; 1];
            let _ = parked.read(&mut byte);
            let _ = exited.send(());
        });

        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(41));
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane_for_key(terminal));
        // Losing the queue is what drops the connection in production.
        drop(sender);
        runtime.drain_events();

        assert!(runtime.connection.is_none());
        assert!(
            reader_exited.recv_timeout(TEST_IO_TIMEOUT).is_ok(),
            "shutdown must release a reader parked on a duplicated descriptor"
        );
        reader.join().expect("reader thread");
    }

    /// B7: a pane producing faster than the parser consumes must not starve the
    /// frame; the overflow stays queued and is announced as pending work.
    #[test]
    fn drain_events_stops_at_the_message_budget_and_reports_pending_work() {
        const EXTRA: usize = 5;

        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(51));
        let pane = pane_for_key(terminal);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 4, cols: 10 });
        let mut sequence = OutputSequence::ZERO;
        for _ in 0..MAX_SERVER_MESSAGES_PER_DRAIN + EXTRA {
            sender
                .send(ServerMessage::PtyOutput {
                    pane,
                    lease: test_lease(),
                    sequence,
                    bytes: Arc::from(b"x".to_vec()),
                })
                .expect("queue output");
            sequence = sequence.checked_add_bytes(1).expect("advance sequence");
        }

        let events = runtime.drain_events();

        assert_eq!(
            events,
            vec![PtyEvent::Output {
                terminal,
                byte_count: MAX_SERVER_MESSAGES_PER_DRAIN,
            }]
        );
        assert!(runtime.has_pending_work());

        let rest = runtime.drain_events();
        assert_eq!(
            rest,
            vec![PtyEvent::Output {
                terminal,
                byte_count: EXTRA,
            }]
        );
        assert!(!runtime.has_pending_work());
    }

    /// B7: the same budget counts bytes, so a few very large chunks cannot
    /// monopolise a frame either.
    #[test]
    fn drain_events_stops_at_the_output_byte_budget() {
        let chunk = MAX_SERVER_OUTPUT_BYTES_PER_DRAIN / 2;
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = PtyKey::Terminal(TerminalId(52));
        let pane = pane_for_key(terminal);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 4, cols: 10 });
        let mut sequence = OutputSequence::ZERO;
        for _ in 0..3 {
            sender
                .send(ServerMessage::PtyOutput {
                    pane,
                    lease: test_lease(),
                    sequence,
                    bytes: Arc::from(vec![b'x'; chunk]),
                })
                .expect("queue output");
            sequence = sequence.checked_add_bytes(chunk).expect("advance sequence");
        }

        let events = runtime.drain_events();

        assert_eq!(
            events,
            vec![PtyEvent::Output {
                terminal,
                byte_count: MAX_SERVER_OUTPUT_BYTES_PER_DRAIN,
            }]
        );
        assert!(runtime.has_pending_work());
    }

    fn unattached_test_runtime(
        client_stream: UnixStream,
        receiver: Receiver<ServerMessage>,
    ) -> PtyRuntime {
        let mut runtime = PtyRuntime::disconnected(unique_socket_path(), Vec::new());
        runtime.connection = Some(ServerConnection {
            writer: Arc::new(Mutex::new(client_stream)),
            receiver,
        });
        runtime.client_scope = Some(test_scope());
        runtime.server_instance = Some(test_server_instance());
        runtime
    }

    fn test_model_session_identity(namespace: u8, token: u8) -> SessionIdentity {
        SessionIdentity {
            namespace: StateNamespace::from_bytes([namespace; 16]).unwrap(),
            token: crate::model::SessionToken::from_bytes([token; 16]).unwrap(),
        }
    }

    fn test_runtime(
        client_stream: UnixStream,
        receiver: Receiver<ServerMessage>,
        terminal: PtyKey,
        pane: PaneId,
    ) -> PtyRuntime {
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        runtime.bind_pane(terminal, pane);
        let entry = runtime.pane_entry(terminal);
        entry.lease = Some(test_lease());
        entry.expected_output = Some(OutputSequence::ZERO);
        runtime
    }

    fn unique_socket_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-pty-test-{unique}.sock"))
    }
}
