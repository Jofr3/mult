use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use mult::cli;
use mult_protocol::{
    bounded_screen_dimensions, default_socket_path, ensure_private_dir,
    peer::verify_peer_is_self,
    read_message,
    shell::{default_shell, display_arg, shell_command_args},
    write_message, ClientMessage, ExitInfo, ForegroundProcessInfo, InstanceId, LaunchSpec,
    PaneInfo, RejectCode, ServerMessage, SessionId, SessionInfo, PROTOCOL_VERSION,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};

type ClientId = u64;
type SharedServer = Arc<Mutex<ServerState>>;
type SharedPane = Arc<Mutex<PaneState>>;
type SharedPtyInput = Arc<PtyInputQueue>;
type SharedMasterPty = Arc<Mutex<Box<dyn MasterPty + Send>>>;
type ClientSender = mpsc::SyncSender<ServerMessage>;

/// Exit code for an unusable command line, matching the client's.
const EXIT_USAGE: u8 = 2;
const CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// Raw PTY bytes retained per pane, sized from what a client can actually show.
///
/// The client replays this history into a vt100 parser that keeps
/// `TERMINAL_SCROLLBACK_LINES` (5 000, see `src/pty.rs`) lines of scrollback, so
/// everything older than 5 000 rendered lines is discarded the moment it
/// arrives. 5 000 lines x 256 columns is 1.25 MiB of glyphs; x4 covers escape
/// sequences, multi-byte UTF-8 and redraw traffic that never becomes a
/// scrollback line at all. 5 MiB is therefore already generous, and keeps ten
/// panes at 50 MiB resident instead of the 320 MiB the previous 32 MiB cap cost.
const RAW_HISTORY_MAX_BYTES: usize = 5 * 1024 * 1024;
const RAW_HISTORY_CHUNK_BYTES: usize = 64 * 1024;
/// `RawHistory` only ever trims sealed chunks, which requires the cap to leave
/// room for at least one whole chunk plus the open tail chunk.
const _: () = assert!(RAW_HISTORY_MAX_BYTES >= 2 * RAW_HISTORY_CHUNK_BYTES);
const CLIENT_QUEUE_CAPACITY: usize = 1_024;
/// Messages queued in front of one pane's PTY master before input is refused.
///
/// The queue exists because writing to a master is a *blocking* operation with
/// no upper bound: the kernel's input buffer for a PTY is a few KiB, and a child
/// that never reads its stdin (a pager waiting on a keypress it will not get, a
/// program that closed stdin) fills it permanently. Doing that write on the
/// connection's reader thread — as this daemon used to — stops the daemon
/// reading the socket, the socket buffer then fills, and the client's render
/// thread blocks forever in its own `write_all`: a two-sided deadlock reachable
/// by pasting a large buffer into such a pane (A2). Each pane therefore owns a
/// writer thread, and this bounded queue is the only thing the socket reader
/// ever touches.
const PTY_INPUT_QUEUE_CAPACITY: usize = 64;
/// Bytes queued in front of one pane's PTY master before input is refused.
///
/// [`PTY_INPUT_QUEUE_CAPACITY`] bounds the queue in *messages*, and a message
/// carries whatever fits in one protocol frame — up to `MAX_MESSAGE_BYTES`
/// (16 MiB). 64 of those is ~1 GiB of resident input per pane, which a same-uid
/// client can park in a pane whose child never reads its stdin, on every one of
/// `MAX_SESSIONS` panes . The byte bound is what actually caps the
/// memory; the message bound stays because it is what keeps a burst of tiny
/// keystrokes from queueing without limit.
///
/// A chunk is admitted unconditionally when the queue is empty, so a legitimate
/// large paste into a healthy pane is never refused for being large — only for
/// arriving on top of input the child has not taken yet. The resident worst case
/// is therefore this cap plus one frame.
const PTY_INPUT_QUEUE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Live PTY bytes one [`ReplayGate`] may hold while a scrollback replay is still
/// being queued.
///
/// The gate exists to order the replay ahead of live output, not to become a
/// second unbounded history: a pane that produces more than this while its
/// replay is in flight has a client that is not draining its queue, which is the
/// case the live broadcast already handles by dropping the client.
const REPLAY_HOLD_MAX_BYTES: usize = RAW_HISTORY_MAX_BYTES;
/// Concurrent client connections. One connection is one thread pair plus a
/// queue, and nothing but the peer-uid check gates who may open one, so a
/// same-uid loop could otherwise exhaust threads and memory (A10).
const MAX_CLIENTS: usize = 64;
/// Live sessions across all instances. Each pins a PTY, a reader thread, a
/// writer thread and up to `RAW_HISTORY_MAX_BYTES`, so the cap is what stops a
/// `CreateSession` loop from taking out every pane the user already has.
const MAX_SESSIONS: usize = 256;
/// How long an established connection may go without sending *anything* before
/// the daemon drops it.
///
/// This is not "how long a client may sit idle": an attached client with no PTY
/// activity for hours is entirely normal. The client sends `Ping` on a much
/// shorter timer (see `PtyRuntime::KEEPALIVE_INTERVAL`), so silence for this
/// long means the peer is gone in a way the socket never reported — the case
/// that used to pin a daemon thread forever, because the read timeout was
/// cleared after the hello (A10). The sessions themselves are untouched: the
/// client reconnects and re-attaches.
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a scrollback replay waits for a backlogged client queue to drain
/// before giving up on the replay. See [`send_pty_scrollback`].
const SCROLLBACK_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const SCROLLBACK_RETRY_INTERVAL: Duration = Duration::from_millis(5);
/// How long everything on a stopped pane's terminal gets to exit after the
/// polite hangup signal, before it is killed outright. See [`kill_and_reap`].
const STOP_GRACE_PERIOD: Duration = Duration::from_millis(250);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone)]
struct ClientHandle {
    id: ClientId,
    sender: ClientSender,
    // A clone of the client socket kept solely so the server can proactively
    // tear the connection down (`shutdown`) when the client falls too far
    // behind or its writer dies. Shutting down any clone of the socket unblocks
    // the reader/writer threads that hold the other clones.
    stream: Arc<UnixStream>,
}

impl ClientHandle {
    /// Hand a message to the client's writer queue without ever blocking the
    /// caller. Returns `false` when the client should be dropped because its
    /// queue is full (it cannot keep up) or its receiver is gone.
    fn try_deliver(&self, message: ServerMessage) -> bool {
        match self.sender.try_send(message) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) | Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    }

    fn disconnect(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// A client attached to one pane, together with the ordering gate for *that*
/// attach. A connection attaches many panes, so the gate belongs here and not on
/// the shared [`ClientHandle`]: one pane's replay must not hold up another's
/// live output.
#[derive(Clone)]
struct PaneClient {
    handle: ClientHandle,
    gate: Arc<ReplayGate>,
}

impl PaneClient {
    fn id(&self) -> ClientId {
        self.handle.id
    }

    /// Deliver a pane-scoped message, or hold it until this client's scrollback
    /// replay has been queued. Never blocks. Returns `false` when the client
    /// must be dropped, exactly as [`ClientHandle::try_deliver`] does.
    fn try_deliver(&self, message: ServerMessage) -> bool {
        self.gate.deliver(&self.handle, message)
    }

    fn disconnect(&self) {
        self.handle.disconnect();
    }
}

/// Orders one client's scrollback replay ahead of the pane's live output (R2).
///
/// `Attach` snapshots the history and registers the client under the pane lock,
/// then queues `Attached` and the replay with that lock *released* — and the
/// pane's reader thread is typically already blocked on it. Without a gate the
/// reader wins the race the moment the lock is dropped: it appends a chunk that
/// is not in the snapshot and `try_send`s it to the just-registered client, so
/// the client reads `PtyOutput(new)`, `Attached`, `PtyScrollback(old)` and feeds
/// both into one parser with no reset (`src/pty.rs`). Attaching to a busy pane —
/// `tail -f`, a running build — then left a permanently mis-ordered screen.
///
/// The gate is installed in `pane.clients` under the same lock hold that takes
/// the snapshot, so every byte is either *in* the snapshot or held here, with no
/// gap and no duplication. Releasing it flushes what it holds in arrival order.
///
/// Concurrency invariants:
/// - The gate mutex is a leaf: it is only ever taken with no pane and no server
///   lock held (every broadcast clones the client list under the pane lock and
///   delivers outside it), and nothing is called under it but non-blocking
///   `try_send`s.
/// - The pane's reader thread is the only producer, so holding the gate mutex
///   across the hold-or-send decision is what makes "held, then live" a total
///   order rather than a race with [`ReplayGate::release`].
struct ReplayGate {
    state: Mutex<ReplayGateState>,
}

enum ReplayGateState {
    /// The replay is not fully queued yet; live messages wait here, in order.
    Pending {
        held: VecDeque<ServerMessage>,
        bytes: usize,
    },
    /// The replay is queued (or was given up on): deliver straight through.
    Open,
}

impl ReplayGate {
    fn pending() -> Self {
        Self {
            state: Mutex::new(ReplayGateState::Pending {
                held: VecDeque::new(),
                bytes: 0,
            }),
        }
    }

    /// A gate that never holds anything, for a client that is not replaying.
    #[cfg(test)]
    fn open() -> Self {
        Self {
            state: Mutex::new(ReplayGateState::Open),
        }
    }

    fn deliver(&self, client: &ClientHandle, message: ServerMessage) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        match &mut *state {
            ReplayGateState::Open => client.try_deliver(message),
            ReplayGateState::Pending { held, bytes } => {
                let payload = message_payload_len(&message);
                if held.len() >= CLIENT_QUEUE_CAPACITY
                    || bytes.saturating_add(payload) > REPLAY_HOLD_MAX_BYTES
                {
                    // Same verdict as a full client queue: this client cannot
                    // keep up, and the pane must not grow a buffer for it.
                    return false;
                }
                *bytes += payload;
                held.push_back(message);
                true
            }
        }
    }

    /// Queue everything the gate held, then let later messages through.
    ///
    /// Called once per attach, by the connection's own thread, after the replay
    /// has been queued — including when the replay gave up part-way, so a pane
    /// never holds messages for a client whose replay will never finish.
    fn release(&self, client: &ClientHandle) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let ReplayGateState::Pending { held, .. } =
            std::mem::replace(&mut *state, ReplayGateState::Open)
        else {
            return true;
        };
        // Still under the gate mutex: a chunk the reader thread is delivering
        // right now lands after these, never between them.
        for message in held {
            if !client.try_deliver(message) {
                return false;
            }
        }
        true
    }
}

/// The payload a message will occupy in a client's queue. Only the two byte-
/// carrying variants are worth counting; everything else is a fixed handful of
/// bytes and is bounded by the message count instead.
fn message_payload_len(message: &ServerMessage) -> usize {
    match message {
        ServerMessage::PtyOutput { bytes, .. } | ServerMessage::PtyScrollback { bytes, .. } => {
            bytes.len()
        }
        _ => 0,
    }
}

/// A session's identity on this daemon: the client instance that owns it plus
/// the id that instance chose (A3).
///
/// Wire session ids come from the client's own `TerminalId`s, so they are only
/// unique within one state file. Keying sessions on the pair is what stops a
/// second `mult` instance from being handed the first one's shell — and, since
/// `Attach` can only name a session in the connection's own namespace, is also
/// what stops a same-uid process that merely speaks the protocol from silently
/// stealing a live PTY stream (C12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SessionKey {
    instance: InstanceId,
    session: SessionId,
}

struct ServerState {
    sessions: BTreeMap<SessionKey, SharedPane>,
    reserved_sessions: BTreeSet<SessionKey>,
    /// Ids of connections currently being served, so the cap counts what is
    /// live and releasing it twice (the writer thread and the reader both reap
    /// a client) is idempotent.
    live_clients: BTreeSet<ClientId>,
    next_session_id: u64,
    next_client_id: ClientId,
}

/// Raw PTY output retained for replay on attach.
///
/// A deque of sealed, refcounted chunks plus one open tail chunk — deliberately
/// not a flat `Vec<u8>`. A flat buffer has to memmove the whole retained history
/// on every read once the cap is reached (measured: 2.27 ms per 8 KiB read at a
/// 32 MiB cap, all of it under the `PaneState` mutex, which stalls attach, input
/// and resize for every client). Here trimming pops whole chunks or advances an
/// offset into the front one, so it costs O(bytes dropped) and never touches the
/// bytes it keeps. Sealed chunks are shared by refcount so an attach can
/// snapshot the history for replay without copying it under the lock.
struct RawHistory {
    /// Full chunks, oldest first. Immutable once sealed, so [`RawHistory::snapshot`]
    /// shares them instead of copying them.
    sealed: VecDeque<Arc<Vec<u8>>>,
    /// Bytes already dropped from the front sealed chunk. Trimming advances this
    /// rather than shifting the chunk's contents. Always 0 when `sealed` is empty.
    head: usize,
    /// The chunk currently being filled; sealed once it is full. Never trimmed:
    /// the cap is at least two chunks, so an open tail is always inside it.
    open: Vec<u8>,
    /// Live bytes across `sealed` (minus `head`) and `open`.
    len: usize,
}

/// A point-in-time view of a pane's history, taken under the pane lock and
/// replayed outside it. Holds refcounted references to the sealed chunks, so
/// taking it is O(chunks) pointer clones plus one copy of the open tail.
struct HistorySnapshot {
    chunks: Vec<Arc<Vec<u8>>>,
    head: usize,
}

/// The bounded, non-blocking way into a pane's PTY master.
///
/// Everything that would block — the actual `write_all` on the master — happens
/// on the pane's own writer thread; the socket reader only ever `try_send`s.
/// When the queue is full the input is refused with an error the client shows in
/// the pane, which is deliberately *not* silent: dropping keystrokes without
/// saying so is indistinguishable from a wedged terminal, and blocking here is
/// the deadlock this exists to prevent (A2).
struct PtyInputQueue {
    sender: mpsc::SyncSender<Vec<u8>>,
    /// Bytes handed to the queue and not yet written to the master. The writer
    /// thread subtracts a chunk only once its `write_all` has returned, so a
    /// chunk stuck in a blocking write still counts against the cap — it is
    /// still resident. Shared with that thread, and deliberately only
    /// approximate under concurrent writers: it is a bound, not an accounting.
    queued_bytes: Arc<AtomicUsize>,
}

struct PaneState {
    session: SessionId,
    /// The instance that owns this pane; only its connections can see it.
    instance: InstanceId,
    name: String,
    title: String,
    rows: u16,
    cols: u16,
    raw_history: RawHistory,
    master: SharedMasterPty,
    writer: SharedPtyInput,
    child_pid: Option<u32>,
    foreground_process: ForegroundProcessInfo,
    child: Option<Box<dyn Child + Send + Sync>>,
    clients: Vec<PaneClient>,
    // Set while a foreground-process poll is already scheduled for this pane so
    // a burst of input coalesces into a single in-flight poller thread instead
    // of spawning one thread per keystroke.
    foreground_poll_scheduled: Arc<AtomicBool>,
}

impl RawHistory {
    fn new() -> Self {
        Self {
            sealed: VecDeque::new(),
            head: 0,
            open: Vec::new(),
            len: 0,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }

    fn append(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            if self.open.is_empty() {
                self.open.reserve_exact(RAW_HISTORY_CHUNK_BYTES);
            }
            let take = (RAW_HISTORY_CHUNK_BYTES - self.open.len()).min(rest.len());
            self.open.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.open.len() == RAW_HISTORY_CHUNK_BYTES {
                // Sealing moves the buffer into the `Arc` instead of copying it.
                self.sealed
                    .push_back(Arc::new(std::mem::take(&mut self.open)));
            }
        }
        self.len += bytes.len();
        self.trim();
    }

    /// Drop the oldest bytes above the cap. Whole chunks are popped; a partial
    /// chunk only advances `head`. Either way the cost is proportional to what
    /// is dropped, never to what is kept.
    fn trim(&mut self) {
        while self.len > RAW_HISTORY_MAX_BYTES {
            // Unreachable in practice: the cap is larger than the open chunk, so
            // an over-cap history always has a sealed chunk to drop.
            let Some(front) = self.sealed.front() else {
                return;
            };
            let live = front.len() - self.head;
            let overflow = self.len - RAW_HISTORY_MAX_BYTES;
            if overflow >= live {
                self.sealed.pop_front();
                self.head = 0;
                self.len -= live;
            } else {
                self.head += overflow;
                self.len -= overflow;
            }
        }
    }

    fn snapshot(&self) -> HistorySnapshot {
        let mut chunks = Vec::with_capacity(self.sealed.len() + 1);
        chunks.extend(self.sealed.iter().map(Arc::clone));
        if !self.open.is_empty() {
            // The open chunk is still being written, so it is the only part of
            // the history a snapshot copies — at most RAW_HISTORY_CHUNK_BYTES.
            chunks.push(Arc::new(self.open.clone()));
        }
        HistorySnapshot {
            chunks,
            head: self.head,
        }
    }
}

impl HistorySnapshot {
    /// The live history in order, as slices of at most `RAW_HISTORY_CHUNK_BYTES`
    /// — exactly the units the scrollback replay sends.
    fn chunks(&self) -> impl Iterator<Item = &[u8]> {
        let head = self.head;
        self.chunks
            .iter()
            .enumerate()
            .map(move |(index, chunk)| {
                if index == 0 {
                    &chunk[head..]
                } else {
                    chunk.as_slice()
                }
            })
            .filter(|chunk| !chunk.is_empty())
    }
}

struct SessionCreateSpec {
    requested_id: Option<SessionId>,
    pane: PaneSpawnSpec,
}

struct PaneSpawnSpec {
    name: String,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    launch: LaunchSpec,
    rows: u16,
    cols: u16,
}

struct SpawnedPane {
    pane: SharedPane,
    reader: Box<dyn Read + Send>,
}

fn main() -> ExitCode {
    // Same rule as the client (E5): every error the operator sees is rendered
    // through `Display`, never `Debug`-printed by the runtime.
    let options = match cli::parse(cli::Binary::Server, env::args_os().skip(1)) {
        Ok(cli::Invocation::Run(options)) => options,
        Ok(cli::Invocation::Print(text)) => {
            print!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("mult-server: {error}");
            eprintln!("try `mult-server --help`");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    match serve(options.socket_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mult-server: {error}");
            ExitCode::FAILURE
        }
    }
}

fn serve(socket_path: Option<PathBuf>) -> io::Result<()> {
    ignore_hangup_signal()?;
    let socket_path = socket_path.unwrap_or_else(default_socket_path);
    bind_socket_path(&socket_path)?;
    let server = Arc::new(Mutex::new(ServerState::default()));
    let listener = bind_unix_listener(&socket_path)?;
    restrict_socket_permissions(&socket_path)?;
    eprintln!("mult-server listening on {}", socket_path.display());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = Arc::clone(&server);
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, server) {
                        eprintln!("client error: {error}");
                    }
                });
            }
            Err(error) => eprintln!("accept error: {error}"),
        }
    }

    Ok(())
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            sessions: BTreeMap::new(),
            reserved_sessions: BTreeSet::new(),
            live_clients: BTreeSet::new(),
            next_session_id: 1,
            next_client_id: 1,
        }
    }
}

/// A daemon-side failure paired with the wire code it is reported as.
///
/// The pairing is made where the failure happens, not where it is sent: before
/// protocol 11 the client recovered the *kind* of failure by substring-matching
/// the rendered message, so rewording a `format!` here silently changed client
/// behaviour (F8). The `message` half stays free prose for the user.
#[derive(Debug)]
struct Rejection {
    code: RejectCode,
    error: io::Error,
}

impl Rejection {
    fn new(code: RejectCode, kind: io::ErrorKind, message: impl Into<String>) -> Self {
        Self {
            code,
            error: io::Error::new(kind, message.into()),
        }
    }

    /// Wrap a plain `io::Error` from a lower layer that has no code of its own.
    fn from_io(code: RejectCode, error: io::Error) -> Self {
        Self { code, error }
    }

    fn send(&self, client: &ClientHandle, pane: Option<SessionId>, context: &str) {
        let _ = client.sender.try_send(ServerMessage::Error {
            pane,
            code: self.code,
            message: if context.is_empty() {
                self.error.to_string()
            } else {
                format!("{context}: {}", self.error)
            },
        });
    }
}

impl ServerState {
    /// Admit a connection, or refuse it because the daemon is already serving
    /// `MAX_CLIENTS`. Refusing is a plain error the caller reports: a daemon
    /// that panicked here would take every live pane with it (A10).
    fn register_client(&mut self) -> Result<ClientId, Rejection> {
        if self.live_clients.len() >= MAX_CLIENTS {
            return Err(Rejection::new(
                RejectCode::ConnectionLimit,
                io::ErrorKind::WouldBlock,
                format!("mult-server is already serving {MAX_CLIENTS} clients"),
            ));
        }
        let id = self.next_client_id;
        self.next_client_id += 1;
        self.live_clients.insert(id);
        Ok(id)
    }

    fn allocate_session_id(&mut self, instance: InstanceId) -> SessionId {
        let key = |session| SessionKey {
            instance,
            session: SessionId(session),
        };
        while self.sessions.contains_key(&key(self.next_session_id))
            || self.reserved_sessions.contains(&key(self.next_session_id))
        {
            self.next_session_id += 1;
        }
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;
        id
    }

    fn reserve_session_id(
        &mut self,
        instance: InstanceId,
        requested_id: Option<SessionId>,
    ) -> Result<SessionKey, Rejection> {
        if self.sessions.len() + self.reserved_sessions.len() >= MAX_SESSIONS {
            return Err(Rejection::new(
                RejectCode::SessionLimit,
                io::ErrorKind::WouldBlock,
                format!("mult-server is already hosting {MAX_SESSIONS} sessions"),
            ));
        }

        let session = requested_id.unwrap_or_else(|| self.allocate_session_id(instance));
        let key = SessionKey { instance, session };
        if self.sessions.contains_key(&key) || !self.reserved_sessions.insert(key) {
            return Err(Rejection::new(
                RejectCode::SessionCreateFailed,
                io::ErrorKind::AlreadyExists,
                format!("session {} already exists or is being created", session.0),
            ));
        }
        Ok(key)
    }

    fn release_session_reservation(&mut self, key: SessionKey) {
        self.reserved_sessions.remove(&key);
    }

    /// The sessions `instance` owns. A connection is never told about — and can
    /// never name — another instance's sessions.
    fn session_infos(&self, instance: InstanceId) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .filter(|(key, _)| key.instance == instance)
            .filter_map(|(_, pane)| pane.lock().ok().map(|pane| pane.session_info()))
            .collect()
    }

    fn session(&self, instance: InstanceId, session: SessionId) -> Option<SharedPane> {
        self.sessions
            .get(&SessionKey { instance, session })
            .cloned()
    }

    /// The pane a message named. A session owns exactly one pane and the two
    /// share an id, so this is simply the session lookup; it used to fall back
    /// to a linear scan that took every pane's mutex looking for a mismatch
    /// `spawn_pane` makes impossible (A11).
    fn pane_by_id(&self, instance: InstanceId, pane: SessionId) -> Option<SharedPane> {
        self.session(instance, pane)
    }

    fn remove_session_if_same(&mut self, key: SessionKey, pane: &SharedPane) -> bool {
        let matches = self
            .sessions
            .get(&key)
            .is_some_and(|existing| Arc::ptr_eq(existing, pane));
        if matches {
            self.sessions.remove(&key);
        }
        matches
    }

    /// Release a connection's slot and detach it from every pane.
    ///
    /// Takes pane locks while holding the server lock, which is why nothing may
    /// hold a pane lock across an unbounded operation — see
    /// [`current_foreground_process`] (R9).
    fn remove_client(&mut self, client_id: ClientId) {
        self.live_clients.remove(&client_id);
        for pane in self.sessions.values() {
            if let Ok(mut pane) = pane.lock() {
                pane.clients.retain(|client| client.id() != client_id);
            }
        }
    }
}

fn ignore_hangup_signal() -> io::Result<()> {
    // The server owns long-lived PTYs. Ignore terminal hangups so a foreground
    // development server, or an autospawned server that has not exec'd yet, does
    // not terminate all panes when the launching terminal is closed.
    if unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn bind_socket_path(path: &PathBuf) -> io::Result<()> {
    match UnixStream::connect(path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("server already listening at {}", path.display()),
            ));
        }
        Err(error) => remove_stale_socket_file(path, error)?,
    }

    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    Ok(())
}

fn bind_unix_listener(path: &Path) -> io::Result<UnixListener> {
    let _umask = UmaskGuard::new(0o077);
    UnixListener::bind(path)
}

struct UmaskGuard {
    previous: libc::mode_t,
}

impl UmaskGuard {
    fn new(mask: libc::mode_t) -> Self {
        Self {
            previous: unsafe { libc::umask(mask) },
        }
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        unsafe {
            libc::umask(self.previous);
        }
    }
}

fn remove_stale_socket_file(path: &PathBuf, connect_error: io::Error) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    if connect_error.kind() == io::ErrorKind::NotFound {
        return Ok(());
    }

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.file_type().is_socket() {
        fs::remove_file(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to remove non-socket path {} after connect failed: {connect_error}",
                path.display()
            ),
        ))
    }
}

fn restrict_socket_permissions(path: &PathBuf) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)
}

fn spawn_pane(key: SessionKey, spec: PaneSpawnSpec) -> io::Result<SpawnedPane> {
    let (rows, cols) = bounded_pty_dimensions(spec.rows, spec.cols);
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(error_to_io)?;

    let shell = default_shell();
    let mut command = CommandBuilder::new(&shell);
    if let LaunchSpec::Command(command_line) = &spec.launch {
        command.args(shell_command_args(command_line.clone()));
    }
    if let Some(cwd) = &spec.cwd {
        command.cwd(cwd.as_os_str());
    }
    // A PTY child talks to mult's built-in vt100 emulator, not to the terminal
    // hosting the `mult` client, so it must advertise *that* emulator's
    // capabilities rather than inherit the host's $TERM. Leaking e.g. TERM=foot
    // makes nvim drive truecolor through foot's terminfo as `\e[38:2::r:g:bm`
    // (colon form with an empty colorspace field) — a shape the emulator does
    // not decode, so every color is dropped and the screen renders monochrome.
    // xterm-256color matches the emulator (xterm-style, 256-color, universally
    // present); COLORTERM=truecolor opts programs into RGB, which they then emit
    // in the semicolon form the emulator understands. Applied before spec.env so
    // a workspace that sets TERM/COLORTERM explicitly still wins.
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    for (key, value) in spec.env {
        command.env(key, value);
    }

    let child = pair.slave.spawn_command(command).map_err(error_to_io)?;
    let child_pid = child.process_id();
    let reader = pair.master.try_clone_reader().map_err(error_to_io)?;
    let writer = spawn_pty_writer(pair.master.take_writer().map_err(error_to_io)?);
    let master = Arc::new(Mutex::new(pair.master));
    let title = pane_title(&shell, &spec.launch);
    let session = key.session;

    let pane = Arc::new(Mutex::new(PaneState {
        session,
        instance: key.instance,
        name: spec.name,
        title,
        rows,
        cols,
        raw_history: RawHistory::new(),
        master,
        writer,
        child_pid,
        foreground_process: ForegroundProcessInfo {
            root_pid: child_pid,
            foreground_pid: None,
            command: None,
        },
        child: Some(child),
        clients: Vec::new(),
        foreground_poll_scheduled: Arc::new(AtomicBool::new(false)),
    }));

    Ok(SpawnedPane { pane, reader })
}

/// Give a pane's master its own writer thread, fed by a bounded queue.
///
/// The thread owns the only handle that ever writes to the master, so the
/// blocking `write_all` happens here and nowhere else. It ends when the pane's
/// queue is dropped (the pane is gone) or the master refuses a write (EIO once
/// the last slave fd closes), which is also what stops it from lingering after
/// a pane exits while a child was still not reading.
fn spawn_pty_writer(mut writer: Box<dyn Write + Send>) -> SharedPtyInput {
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(PTY_INPUT_QUEUE_CAPACITY);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let written = Arc::clone(&queued_bytes);
    thread::spawn(move || {
        for bytes in receiver {
            let len = bytes.len();
            let result = writer.write_all(&bytes).and_then(|()| writer.flush());
            // After the write, not before: bytes blocked in `write_all` are
            // still resident and must still count against the byte cap.
            written.fetch_sub(len, Ordering::AcqRel);
            if result.is_err() {
                break;
            }
        }
    });
    Arc::new(PtyInputQueue {
        sender,
        queued_bytes,
    })
}

/// A registered connection slot, released when this value is dropped.
///
/// `register_client` used to be paired with a `remove_client` at the far end of
/// `handle_client`, below two fallible `try_clone`s. `try_clone` is `dup(2)` and
/// fails on `EMFILE`, so a transient fd-exhaustion burst returned early and
/// leaked a slot per connection — permanently, until the cap refused every new
/// connection with `ConnectionLimit` and the only cure was restarting the
/// daemon, which destroys every live pane (R8). Registration and release are now
/// symmetric on every exit path by construction.
struct ClientSlot {
    id: ClientId,
    server: SharedServer,
}

impl ClientSlot {
    fn new(id: ClientId, server: &SharedServer) -> Self {
        Self {
            id,
            server: Arc::clone(server),
        }
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        // Idempotent: the writer thread releases the same slot when its socket
        // dies, so that a dead connection stops being broadcast to promptly.
        if let Ok(mut server) = self.server.lock() {
            server.remove_client(self.id);
        }
    }
}

fn handle_client(stream: UnixStream, server: SharedServer) -> io::Result<()> {
    handle_client_with_cloner(stream, server, UnixStream::try_clone)
}

/// `clone_stream` is [`UnixStream::try_clone`] in production, and an injected
/// failure in the R8 regression test: it is `dup(2)`, it fails on `EMFILE`, and
/// the connection slot taken just above it must be released even then.
fn handle_client_with_cloner(
    mut stream: UnixStream,
    server: SharedServer,
    clone_stream: impl Fn(&UnixStream) -> io::Result<UnixStream>,
) -> io::Result<()> {
    verify_peer_is_self(&stream, "client")?;
    let (sender, receiver) = mpsc::sync_channel(CLIENT_QUEUE_CAPACITY);
    let client_id = match server.lock().map_err(lock_error)?.register_client() {
        Ok(client_id) => client_id,
        Err(rejection) => {
            // Over the connection cap. Say so on the socket the client already
            // has — it has not sent a hello yet, so this is the only channel —
            // and let the connection close. Nothing else is allocated for it.
            let _ = write_message(
                &mut stream,
                &ServerMessage::Error {
                    pane: None,
                    code: rejection.code,
                    message: rejection.error.to_string(),
                },
            );
            return Ok(());
        }
    };
    // Taken immediately after registration and before anything that can fail, so
    // the slot is released on every path out of this function (R8).
    let _slot = ClientSlot::new(client_id, &server);
    let shutdown_handle = Arc::new(clone_stream(&stream)?);
    let client = ClientHandle {
        id: client_id,
        sender: sender.clone(),
        stream: Arc::clone(&shutdown_handle),
    };

    let mut writer_stream = clone_stream(&stream)?;
    let writer_server = Arc::clone(&server);
    // Detached: it exits on its own when its write fails or the channel closes.
    let _writer = thread::spawn(move || {
        for message in receiver {
            if write_message(&mut writer_stream, &message).is_err() {
                break;
            }
        }
        // The writer is gone (socket error or the channel was dropped). Stop the
        // server from broadcasting into a dead connection and unblock the reader
        // half so this client is fully reaped instead of lingering in panes.
        if let Ok(mut server) = writer_server.lock() {
            server.remove_client(client_id);
        }
        let _ = shutdown_handle.shutdown(Shutdown::Both);
    });

    let result = handle_client_messages(stream, &server, client);
    drop(sender);
    // `_slot` releases the connection slot and detaches this client from every
    // pane as it goes out of scope here.
    result
}

fn handle_client_messages(
    mut stream: UnixStream,
    server: &SharedServer,
    client: ClientHandle,
) -> io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_HELLO_TIMEOUT))?;
    let ClientRead::Message(message) = read_client_message(&mut stream)? else {
        return Ok(());
    };
    // Not `None`: an established connection still has a deadline, it is just a
    // far longer one that the client's keepalive keeps resetting (A10).
    stream.set_read_timeout(Some(CLIENT_IDLE_TIMEOUT))?;

    let ClientMessage::Hello {
        protocol_version,
        instance,
    } = message
    else {
        let _ = client.sender.try_send(ServerMessage::Error {
            pane: None,
            code: RejectCode::HelloRequired,
            message: "expected protocol hello before other client messages".to_string(),
        });
        return Ok(());
    };

    if !send_hello_response(&client, protocol_version, instance) {
        return Ok(());
    }

    loop {
        let message = match read_client_message(&mut stream)? {
            ClientRead::Message(message) => message,
            ClientRead::Disconnected => break,
            ClientRead::Idle => {
                // Nothing at all for `CLIENT_IDLE_TIMEOUT`, not even a
                // keepalive: treat the peer as gone and reclaim the thread. The
                // client's sessions are untouched, so a client that is merely
                // wedged reconnects and re-attaches.
                let _ = client.sender.try_send(ServerMessage::Error {
                    pane: None,
                    code: RejectCode::Unspecified,
                    message: format!(
                        "connection closed after {CLIENT_IDLE_TIMEOUT:?} with no client traffic"
                    ),
                });
                break;
            }
        };
        match message {
            ClientMessage::Hello {
                protocol_version,
                instance: presented,
            } => {
                if presented != instance {
                    // Re-keying a live connection would orphan whatever it
                    // already attached in the old namespace.
                    let _ = client.sender.try_send(ServerMessage::Error {
                        pane: None,
                        code: RejectCode::InstanceMismatch,
                        message: "cannot change instance token on an established connection"
                            .to_string(),
                    });
                    break;
                }
                if !send_hello_response(&client, protocol_version, instance) {
                    break;
                }
            }
            // Traffic is the whole point: reading it already reset the deadline.
            ClientMessage::Ping => {}
            ClientMessage::ListSessions => {
                let sessions = server.lock().map_err(lock_error)?.session_infos(instance);
                let _ = client.sender.try_send(ServerMessage::Sessions(sessions));
            }
            ClientMessage::CreateSession {
                requested_id,
                name,
                cwd,
                env,
                launch,
                rows,
                cols,
            } => {
                let created = create_session(
                    server,
                    instance,
                    SessionCreateSpec {
                        requested_id,
                        pane: PaneSpawnSpec {
                            name,
                            cwd,
                            env,
                            launch,
                            rows,
                            cols,
                        },
                    },
                );
                // A failed create (a duplicate id, a PTY that could not be
                // opened) is scoped to this request. Report it and keep serving:
                // tearing the connection down here would disconnect every other
                // pane the client has open and force a full parser reset.
                let pane = match created {
                    Ok(pane) => pane,
                    Err(rejection) => {
                        // The pane id of a session is its session id, so a
                        // client that named the id it wanted can attribute the
                        // failure to the terminal it was creating.
                        rejection.send(&client, requested_id, "failed to create session");
                        continue;
                    }
                };
                let info = pane.lock().map_err(lock_error)?.session_info();
                let _ = client.sender.try_send(ServerMessage::Sessions(vec![info]));
            }
            ClientMessage::Attach {
                session,
                rows,
                cols,
            } => {
                let pane = {
                    let server = server.lock().map_err(lock_error)?;
                    server.session(instance, session)
                };
                let Some(pane) = pane else {
                    // This instance has no such session: it is gone (the daemon
                    // was restarted or autospawned fresh while the client was
                    // away), or it never belonged to this instance in the first
                    // place — another instance's identically-numbered session is
                    // invisible here, which is the point of the namespace (A3).
                    // Report the pane as exited so a reconnecting client stops
                    // treating it as live and can recover, instead of silently
                    // freezing on a session it will never hear from.
                    let _ = client.sender.try_send(ServerMessage::PaneExited {
                        pane: session,
                        exit: ExitInfo {
                            code: 1,
                            signal: Some("server session unavailable".to_string()),
                        },
                    });
                    continue;
                };

                if let Err(error) = attach_pane(&client, &pane, session, rows, cols, || {}) {
                    // The pane's master is gone or poisoned. That is this
                    // pane's problem, not the connection's.
                    let _ = client.sender.try_send(ServerMessage::Error {
                        pane: Some(session),
                        code: RejectCode::PaneOperationFailed,
                        message: format!("failed to attach session {}: {error}", session.0),
                    });
                }
            }
            ClientMessage::Input { pane, bytes } => {
                let target = {
                    server
                        .lock()
                        .map_err(lock_error)?
                        .pane_by_id(instance, pane)
                };
                if let Some(target) = target {
                    let writer = { Arc::clone(&target.lock().map_err(lock_error)?.writer) };
                    // EIO here means the child on the other end is gone. The
                    // reader thread reports the exit; all this connection needs
                    // is to hear about the pane, not to be torn down.
                    match write_pty_input(&writer, &bytes) {
                        Ok(()) => {
                            if input_may_change_foreground(&bytes) {
                                schedule_foreground_process_poll(target);
                            }
                        }
                        Err(rejection) => {
                            report_pane_error(&client, pane, "write input to", &rejection)
                        }
                    }
                }
            }
            ClientMessage::Resize { pane, rows, cols } => {
                let target = {
                    server
                        .lock()
                        .map_err(lock_error)?
                        .pane_by_id(instance, pane)
                };
                if let Some(target) = target {
                    let result = target.lock().map_err(lock_error)?.resize(rows, cols);
                    if let Err(error) = result {
                        report_pane_error(
                            &client,
                            pane,
                            "resize",
                            &Rejection::from_io(RejectCode::PaneOperationFailed, error),
                        );
                    }
                }
            }
            ClientMessage::Detach => break,
            ClientMessage::Stop { pane } => {
                let target = {
                    server
                        .lock()
                        .map_err(lock_error)?
                        .pane_by_id(instance, pane)
                };
                let Some(target) = target else {
                    // A stale pane id (the client missed the exit, or the daemon
                    // was restarted under it) is a per-pane error, not grounds
                    // for dropping this connection's other panes.
                    let _ = client.sender.try_send(ServerMessage::Error {
                        pane: Some(pane),
                        code: RejectCode::UnknownSession,
                        message: format!("cannot stop unknown pane {}", pane.0),
                    });
                    continue;
                };

                // Take the child out under a brief lock, then kill+reap with the
                // pane lock released so the blocking wait never stalls the reader
                // thread. Remove the session by identity so a recycled id created
                // in the meantime is never torn down by mistake.
                let (key, child, master) = {
                    let mut target = target.lock().map_err(lock_error)?;
                    (
                        target.session_key(),
                        target.take_child(),
                        Arc::clone(&target.master),
                    )
                };
                let foreground_group = foreground_process_group(&master);
                match kill_and_reap(child, foreground_group) {
                    Ok(()) => {
                        server
                            .lock()
                            .map_err(lock_error)?
                            .remove_session_if_same(key, &target);
                    }
                    Err(error) => {
                        let _ = client.sender.try_send(ServerMessage::Error {
                            pane: Some(pane),
                            code: RejectCode::PaneOperationFailed,
                            message: format!("failed to stop pane: {error}"),
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

/// What one read of the client socket produced.
enum ClientRead {
    Message(ClientMessage),
    /// The peer closed (or reset) the connection.
    Disconnected,
    /// The read deadline expired with nothing to show for it. The caller must
    /// close the connection rather than read again: the deadline can also
    /// expire *mid-frame*, which leaves the stream unparseable — and a peer
    /// that takes two minutes over one frame is gone by any definition.
    Idle,
}

fn read_client_message(stream: &mut UnixStream) -> io::Result<ClientRead> {
    match read_message::<ClientMessage>(stream) {
        Ok(message) => Ok(ClientRead::Message(message)),
        Err(error) if is_client_disconnect(&error) => Ok(ClientRead::Disconnected),
        Err(error) if is_read_timeout(&error) => Ok(ClientRead::Idle),
        Err(error) => Err(error),
    }
}

fn is_client_disconnect(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    )
}

/// A socket read deadline expiring. The kind differs by platform — Linux
/// reports `WouldBlock`, others `TimedOut` — and both mean the same thing here.
fn is_read_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn send_hello_response(client: &ClientHandle, protocol_version: u16, instance: InstanceId) -> bool {
    if !instance.is_set() {
        let _ = client.sender.try_send(ServerMessage::Error {
            pane: None,
            code: RejectCode::InstanceTokenRequired,
            message: "client did not present an instance token; upgrade the mult client"
                .to_string(),
        });
        return false;
    }

    if protocol_version != PROTOCOL_VERSION {
        let _ = client.sender.try_send(ServerMessage::Error {
            pane: None,
            code: RejectCode::ProtocolMismatch,
            message: format!(
                "client protocol version {protocol_version} is incompatible with server version {PROTOCOL_VERSION}; restart mult clients"
            ),
        });
        return false;
    }

    let _ = client.sender.try_send(ServerMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
    });
    true
}

fn create_session(
    server: &SharedServer,
    instance: InstanceId,
    spec: SessionCreateSpec,
) -> Result<SharedPane, Rejection> {
    create_session_with_spawner(server, instance, spec, spawn_pane)
}

/// Create a session in `instance`'s namespace.
///
/// A `requested_id` that already exists *in this instance* returns the existing
/// pane: that is the reconnect path, and it is what lets a restarted client
/// reclaim its own terminals. The same id in another instance is a different
/// session entirely and is created fresh, which is the whole point of A3 — the
/// old global map handed the second `mult` window the first one's shell.
fn create_session_with_spawner(
    server: &SharedServer,
    instance: InstanceId,
    spec: SessionCreateSpec,
    spawn: impl FnOnce(SessionKey, PaneSpawnSpec) -> io::Result<SpawnedPane>,
) -> Result<SharedPane, Rejection> {
    let key = {
        let mut server = server.lock().map_err(pane_lock_rejection)?;
        if let Some(requested_id) = spec.requested_id {
            if let Some(existing) = server.session(instance, requested_id) {
                return Ok(existing);
            }
        }
        server.reserve_session_id(instance, spec.requested_id)?
    };
    let session = key.session;

    let spawned = match spawn(key, spec.pane) {
        Ok(spawned) => spawned,
        Err(error) => {
            if let Ok(mut server) = server.lock() {
                server.release_session_reservation(key);
            }
            return Err(Rejection::from_io(RejectCode::SessionCreateFailed, error));
        }
    };

    {
        let mut server = server.lock().map_err(pane_lock_rejection)?;
        server.release_session_reservation(key);
        if server.sessions.contains_key(&key) {
            let child = spawned
                .pane
                .lock()
                .ok()
                .and_then(|mut pane| pane.take_child());
            drop(server);
            // Nothing has run on this PTY yet, so there is no foreground job
            // group to reach besides the freshly spawned shell's own.
            let _ = kill_and_reap(child, None);
            return Err(Rejection::new(
                RejectCode::SessionCreateFailed,
                io::ErrorKind::AlreadyExists,
                format!("session {} was created concurrently", session.0),
            ));
        }
        server.sessions.insert(key, Arc::clone(&spawned.pane));
    }
    spawn_reader(
        spawned.reader,
        Arc::clone(&spawned.pane),
        Arc::clone(server),
    );
    Ok(spawned.pane)
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, pane: SharedPane, server: SharedServer) {
    thread::spawn(move || {
        let mut buffer = [0; 8192];
        // Reused across reads: the client snapshot has to leave the pane lock,
        // but it does not have to allocate a fresh Vec per 8 KiB of output.
        let mut clients: Vec<PaneClient> = Vec::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = &buffer[..n];
                    let pane_id = match pane.lock() {
                        Ok(mut locked) => {
                            locked.raw_history.append(chunk);
                            clients.clear();
                            clients.extend(locked.clients.iter().cloned());
                            locked.session
                        }
                        Err(_) => break,
                    };
                    // The copy into a message payload only happens when someone
                    // is actually attached, and the last (usually only) client
                    // takes it by move rather than by clone.
                    if !clients.is_empty() {
                        let dropped = broadcast_pty_output(pane_id, chunk.to_vec(), &clients);
                        remove_pane_clients(&pane, &dropped);
                    }
                    // Polling the foreground process costs a pane lock, a master
                    // lock, a tcgetpgrp ioctl and a /proc read; per 8 KiB read
                    // that is thousands per second under load. Route it through
                    // the same debounced poller the input path uses, which
                    // coalesces a burst of reads into a single staged poll.
                    schedule_foreground_process_poll(Arc::clone(&pane));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    eprintln!("failed to read PTY output: {error}");
                    break;
                }
            }
        }

        let Some((key, pane_id, clients, exit)) = pane_exit(&pane) else {
            return;
        };
        if let Ok(mut server) = server.lock() {
            server.remove_session_if_same(key, &pane);
        }
        let _ = broadcast_exit(pane_id, exit, &clients);
    });
}

fn pane_exit(pane: &SharedPane) -> Option<(SessionKey, SessionId, Vec<PaneClient>, ExitInfo)> {
    let (session, pane_id, clients, mut child) = {
        let mut pane = pane.lock().ok()?;
        let child = pane.child.take()?;
        (
            pane.session_key(),
            pane.session,
            pane.clients.clone(),
            child,
        )
    };

    let exit = match child.try_wait() {
        Ok(Some(status)) => exit_info(status),
        Ok(None) => match child.wait() {
            Ok(status) => exit_info(status),
            Err(error) => {
                eprintln!("failed to wait for PTY child: {error}");
                ExitInfo {
                    code: 1,
                    signal: None,
                }
            }
        },
        Err(error) => {
            eprintln!("failed to poll PTY child exit: {error}");
            ExitInfo {
                code: 1,
                signal: None,
            }
        }
    };

    Some((session, pane_id, clients, exit))
}

fn exit_info(status: ExitStatus) -> ExitInfo {
    ExitInfo {
        code: status.exit_code(),
        signal: status.signal().map(ToOwned::to_owned),
    }
}

/// Attach `client` to `pane`: take over the pane, confirm, and replay its
/// history — in that order, and with nothing of the pane's live output allowed
/// to overtake the replay (R2).
///
/// The ordering is the whole point of this function. Under one hold of the pane
/// lock it resizes, installs the client behind a *pending* [`ReplayGate`] and
/// snapshots the history; every byte the pane produces from then on is either
/// already in that snapshot or held by the gate. The lock is then released — it
/// must be, because the replay can wait seconds for a backlogged client and the
/// pane's reader thread needs the lock to keep draining — and the gate is
/// released once the replay has been queued.
///
/// `after_register` runs in exactly the window the reader thread used to win:
/// the pane lock released, the client registered, the replay not yet queued. It
/// is `|| {}` in production and the interleaving point the regression test
/// drives.
fn attach_pane(
    client: &ClientHandle,
    pane: &SharedPane,
    session: SessionId,
    rows: u16,
    cols: u16,
    after_register: impl FnOnce(),
) -> io::Result<()> {
    let gate = Arc::new(ReplayGate::pending());
    let attached = {
        let mut locked = pane
            .lock()
            .map_err(|_| io::Error::other("pane lock poisoned"))?;
        locked.resize(rows, cols).map(|()| {
            let evicted = locked.attach_client(PaneClient {
                handle: client.clone(),
                gate: Arc::clone(&gate),
            });
            (
                locked.pane_info(),
                locked.session,
                // Sealed history chunks are shared by refcount, so the replay
                // snapshot costs a handful of pointer clones instead of a
                // multi-megabyte memcpy held under the pane mutex.
                locked.raw_history.snapshot(),
                Arc::clone(&locked.master),
                locked.child_pid,
                evicted,
            )
        })?
    };
    let (pane_info, pane_id, history, master, child_pid, evicted) = attached;
    after_register();

    notify_evicted_clients(&evicted, pane_id);
    let _ = client.sender.try_send(ServerMessage::Attached {
        session,
        panes: vec![pane_info],
    });

    // Read with the pane lock released, and record the result under a second,
    // brief hold: this reads `/proc` (R9, see `current_foreground_process`).
    let process = current_foreground_process(&master, child_pid);
    if let Ok(mut locked) = pane.lock() {
        locked.foreground_process = process.clone();
    }
    let _ = client.sender.try_send(ServerMessage::ForegroundProcess {
        pane: pane_id,
        process,
    });

    send_pty_scrollback(client, pane_id, &history);
    // Released even when the replay gave up part-way, so a pane never holds
    // output for a client whose replay will never finish. A client that cannot
    // take what the gate held is treated exactly as a client that cannot keep up
    // with the live broadcast: dropped from the pane and disconnected.
    if !gate.release(client) {
        client.disconnect();
        remove_pane_clients(pane, &[client.id]);
    }
    Ok(())
}

/// Replay a pane's retained history to the client that just attached.
///
/// Deliberately not the eviction path the live broadcast uses. A replay is many
/// messages at once (up to `RAW_HISTORY_MAX_BYTES / RAW_HISTORY_CHUNK_BYTES` of
/// them), so a client carrying any pre-existing backlog would overflow its queue
/// and be disconnected by the very attach it just asked for — then reconnect,
/// re-attach and overflow again. Instead the replay waits for the writer thread
/// to drain, and gives up on the replay alone (never on the connection) if the
/// client has not moved a single message within the deadline.
///
/// The deadline is for the whole replay, not for each chunk: a full 5 MiB
/// history is 80 chunks, and a per-chunk deadline let one attach hold the
/// connection's reader thread for 80 x `SCROLLBACK_SEND_TIMEOUT` — around 400 s,
/// during which that client's input, resizes and pings go unread .
fn send_pty_scrollback(client: &ClientHandle, pane: SessionId, history: &HistorySnapshot) {
    send_pty_scrollback_until(
        client,
        pane,
        history,
        Instant::now() + SCROLLBACK_SEND_TIMEOUT,
    );
}

fn send_pty_scrollback_until(
    client: &ClientHandle,
    pane: SessionId,
    history: &HistorySnapshot,
    deadline: Instant,
) {
    let mut sent_any = false;
    for chunk in history.chunks() {
        sent_any = true;
        if !deliver_scrollback_chunk(client, pane, chunk.to_vec(), deadline) {
            return;
        }
    }

    if !sent_any {
        // A pane with no history still gets exactly one scrollback message, so a
        // client can tell "replay finished" from "replay still coming".
        deliver_scrollback_chunk(client, pane, Vec::new(), deadline);
    }
}

fn deliver_scrollback_chunk(
    client: &ClientHandle,
    pane: SessionId,
    bytes: Vec<u8>,
    deadline: Instant,
) -> bool {
    let mut message = ServerMessage::PtyScrollback { pane, bytes };
    loop {
        match client.sender.try_send(message) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(returned)) => {
                if Instant::now() >= deadline {
                    return false;
                }
                // `try_send` hands the message back, so retrying costs nothing.
                message = returned;
                thread::sleep(SCROLLBACK_RETRY_INTERVAL);
            }
        }
    }
}

/// Tell the clients that just lost a pane to a takeover that it is gone.
///
/// The pane is single-attach with takeover, and an evicted client used to be
/// dropped from the pane's list in silence: it kept listing the session and
/// rendering a terminal that would never update again. `PaneExited` retires it
/// instead. A client that cannot even be told (its queue is wedged or its
/// receiver is gone) is disconnected so its threads and fds are reclaimed;
/// one that takes the message keeps its *other* panes, which are unaffected by
/// this takeover.
fn notify_evicted_clients(evicted: &[PaneClient], pane: SessionId) {
    for client in evicted {
        let notified = client.try_deliver(ServerMessage::Error {
            pane: Some(pane),
            code: RejectCode::SessionBusy,
            message: format!("pane {} was taken over by another mult client", pane.0),
        }) && client.try_deliver(ServerMessage::PaneExited {
            pane,
            exit: ExitInfo {
                code: 0,
                signal: Some("detached: another client attached to this pane".to_string()),
            },
        });
        if !notified {
            client.disconnect();
        }
    }
}

fn broadcast_foreground_process_if_changed(pane: &SharedPane) {
    let Some((pane_id, process, clients)) = foreground_process_update(pane) else {
        return;
    };

    let dropped = deliver_to_clients(&clients, |client| {
        client.try_deliver(ServerMessage::ForegroundProcess {
            pane: pane_id,
            process: process.clone(),
        })
    });
    remove_pane_clients(pane, &dropped);
}

/// Poll a pane's foreground process, and report it plus the clients to tell when
/// it has changed.
///
/// Two brief pane-lock holds with the poll *between* them, never one hold across
/// it: the poll reads `/proc` and a filesystem read is not bounded in time (R9,
/// see [`current_foreground_process`]). Two concurrent pollers can therefore
/// interleave and store the older reading; the next poll corrects it, and the
/// staged poller already coalesces bursts into one in-flight thread per pane.
fn foreground_process_update(
    pane: &SharedPane,
) -> Option<(SessionId, ForegroundProcessInfo, Vec<PaneClient>)> {
    let (master, child_pid) = {
        let locked = pane.lock().ok()?;
        (Arc::clone(&locked.master), locked.child_pid)
    };
    let process = current_foreground_process(&master, child_pid);
    let mut locked = pane.lock().ok()?;
    if process == locked.foreground_process {
        return None;
    }
    locked.foreground_process = process.clone();
    Some((locked.session, process, locked.clients.clone()))
}

fn schedule_foreground_process_poll(pane: SharedPane) {
    let scheduled = match pane.lock() {
        Ok(pane) => Arc::clone(&pane.foreground_poll_scheduled),
        Err(_) => return,
    };
    // Coalesce bursts of input into a single in-flight poller. When a poll is
    // already scheduled it will observe the latest state at its staged
    // intervals, so a paste of many newlines no longer spawns one thread per
    // byte (and one stuck /proc read can no longer multiply across threads).
    if scheduled.swap(true, Ordering::AcqRel) {
        return;
    }

    thread::spawn(move || {
        for delay in [
            Duration::from_millis(25),
            Duration::from_millis(100),
            Duration::from_millis(500),
        ] {
            thread::sleep(delay);
            broadcast_foreground_process_if_changed(&pane);
        }
        scheduled.store(false, Ordering::Release);
    });
}

fn input_may_change_foreground(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|byte| matches!(*byte, b'\r' | b'\n' | 0x03 | 0x1a))
}

/// Deliver a per-client message without ever blocking the caller, returning the
/// ids of clients that must be dropped because they could not keep up or are
/// gone. Each dropped client is disconnected so its reader/writer threads exit
/// and the connection is reclaimed; the caller is responsible for removing the
/// returned ids from the pane's client list.
fn deliver_to_clients(
    clients: &[PaneClient],
    mut deliver: impl FnMut(&PaneClient) -> bool,
) -> Vec<ClientId> {
    let mut dropped = Vec::new();
    for client in clients {
        if !deliver(client) {
            client.disconnect();
            dropped.push(client.id());
        }
    }
    dropped
}

/// Report a failure that is scoped to one pane. The connection stays up: a
/// single dead pane must not disconnect every other pane on it and force the
/// client through a full reconnect and parser reset.
fn report_pane_error(client: &ClientHandle, pane: SessionId, action: &str, rejection: &Rejection) {
    let _ = client.sender.try_send(ServerMessage::Error {
        pane: Some(pane),
        code: rejection.code,
        message: format!("failed to {action} pane {}: {}", pane.0, rejection.error),
    });
}

fn remove_pane_clients(pane: &SharedPane, dropped: &[ClientId]) {
    if dropped.is_empty() {
        return;
    }
    if let Ok(mut pane) = pane.lock() {
        pane.clients
            .retain(|client| !dropped.contains(&client.id()));
    }
}

fn broadcast_exit(pane: SessionId, exit: ExitInfo, clients: &[PaneClient]) -> Vec<ClientId> {
    deliver_to_clients(clients, |client| {
        client.try_deliver(ServerMessage::PaneExited {
            pane,
            exit: exit.clone(),
        })
    })
}

/// Broadcast one PTY chunk. The last client takes the payload by move, so the
/// normal single-client case hands the bytes straight to the serializer instead
/// of copying them once more per client.
fn broadcast_pty_output(
    pane: SessionId,
    mut bytes: Vec<u8>,
    clients: &[PaneClient],
) -> Vec<ClientId> {
    let last = clients.len().saturating_sub(1);
    let mut dropped = Vec::new();
    for (index, client) in clients.iter().enumerate() {
        let payload = if index == last {
            std::mem::take(&mut bytes)
        } else {
            bytes.clone()
        };
        if !client.try_deliver(ServerMessage::PtyOutput {
            pane,
            bytes: payload,
        }) {
            client.disconnect();
            dropped.push(client.id());
        }
    }
    dropped
}

impl PaneState {
    fn session_key(&self) -> SessionKey {
        SessionKey {
            instance: self.instance,
            session: self.session,
        }
    }

    fn session_info(&self) -> SessionInfo {
        SessionInfo {
            id: self.session,
            name: self.name.clone(),
            attached: !self.clients.is_empty(),
        }
    }

    fn pane_info(&self) -> PaneInfo {
        PaneInfo {
            id: self.session,
            title: self.title.clone(),
            rows: self.rows,
            cols: self.cols,
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        let (rows, cols) = bounded_pty_dimensions(rows, cols);
        self.rows = rows;
        self.cols = cols;
        self.master
            .lock()
            .map_err(|_| io::Error::other("PTY master lock poisoned"))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(error_to_io)
    }

    fn take_child(&mut self) -> Option<Box<dyn Child + Send + Sync>> {
        self.child.take()
    }

    /// Attach `client`, taking over from any other client (single-attach with
    /// takeover). This lets a reconnecting client re-attach even when its
    /// previous, now-dead connection is still listed here and has not been
    /// reaped yet, instead of being rejected as already attached.
    ///
    /// Returns the handles that were evicted; the caller must tell them (see
    /// [`notify_evicted_clients`]) rather than drop them in silence.
    fn attach_client(&mut self, client: PaneClient) -> Vec<PaneClient> {
        let mut evicted = Vec::new();
        let id = client.id();
        self.clients.retain(|existing| {
            if existing.id() == id {
                return true;
            }
            evicted.push(existing.clone());
            false
        });
        // A re-attach on a connection that is already here replaces its entry,
        // so the gate in the list is always the one the *current* attach will
        // release; leaving the old one would hold the pane's output for a replay
        // that has already finished.
        if let Some(existing) = self.clients.iter_mut().find(|existing| existing.id() == id) {
            *existing = client;
        } else {
            self.clients.push(client);
        }
        evicted
    }
}

/// Kill and reap a PTY child *and everything else on its terminal*.
///
/// `wait` can block, so this must run with the `PaneState` mutex released — the
/// per-pane reader thread needs that lock to keep draining output and would
/// otherwise stall behind us.
///
/// Signalling only `child` (SIGKILL to a single pid) is not enough: the shell
/// runs each job in its own process group, so a pager, a dev server or an `ssh`
/// started inside the pane survives, keeps the PTY slave open, and the reader
/// thread therefore never reaches EOF — leaking the thread, the master fd and
/// the pane's retained history for the lifetime of the daemon. So signal both
/// the session leader's group and the terminal's current foreground group:
/// SIGHUP first (what a real terminal hangup delivers, and what lets programs
/// exit cleanly), then SIGKILL for whatever is still alive after the grace
/// period. Once the last slave fd closes, the reader ends and the pane's
/// resources are reclaimed.
fn kill_and_reap(
    child: Option<Box<dyn Child + Send + Sync>>,
    foreground_group: Option<libc::pid_t>,
) -> io::Result<()> {
    let Some(mut child) = child else {
        return Ok(());
    };

    let groups = stop_process_groups(child.process_id(), foreground_group);
    if groups.is_empty() {
        // No usable pid to derive a group from: fall back to the single-process
        // kill rather than signalling nothing at all.
        child.kill()?;
        let _ = child.wait();
        return Ok(());
    }

    signal_process_groups(&groups, libc::SIGHUP);
    if !wait_for_process_groups_to_exit(child.as_mut(), &groups, STOP_GRACE_PERIOD) {
        signal_process_groups(&groups, libc::SIGKILL);
    }
    let _ = child.wait();
    Ok(())
}

/// The process groups to signal when stopping a pane: the child's own group (it
/// is spawned as a session leader, so its pid is its process group id) plus the
/// terminal's foreground group, which is a *different* group whenever the shell
/// has a job running. The daemon's own group is never included, so a bad pid can
/// never make the server signal itself.
fn stop_process_groups(
    child_pid: Option<u32>,
    foreground_group: Option<libc::pid_t>,
) -> Vec<libc::pid_t> {
    // SAFETY: `getpgrp` cannot fail and touches no memory we own.
    let own_group = unsafe { libc::getpgrp() };
    let child_group = child_pid.and_then(|pid| libc::pid_t::try_from(pid).ok());
    let mut groups = Vec::new();
    for group in [child_group, foreground_group].into_iter().flatten() {
        if group > 1 && group != own_group && !groups.contains(&group) {
            groups.push(group);
        }
    }
    groups
}

fn signal_process_groups(groups: &[libc::pid_t], signal: libc::c_int) {
    for group in groups {
        // SAFETY: `killpg` only inspects the process table; failure (typically
        // ESRCH, the group is already gone) is reported by the return value,
        // which is exactly the case we want to ignore.
        let _ = unsafe { libc::killpg(*group, signal) };
    }
}

/// Poll until nothing is left in `groups`, reaping the pane's own child along
/// the way — a zombie is still a member of its process group, so the child has
/// to be reaped before the group can ever look empty. Returns false if anything
/// is still alive at the deadline, i.e. the polite signal was not enough.
///
/// Waiting on the *group* rather than on the child is the point: the child
/// usually exits on SIGHUP immediately, and it is the grandchildren that
/// outlive it and keep the PTY slave open.
fn wait_for_process_groups_to_exit(
    child: &mut (dyn Child + Send + Sync),
    groups: &[libc::pid_t],
    grace: Duration,
) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        // An error from `try_wait` is unrecoverable here; treat the child as
        // reaped and let the group check decide.
        let child_reaped = !matches!(child.try_wait(), Ok(None));
        if child_reaped && !groups.iter().copied().any(process_group_exists) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }
}

fn process_group_exists(group: libc::pid_t) -> bool {
    // SAFETY: signal 0 delivers nothing; `killpg` only performs the existence
    // and permission checks and reports them through the return value.
    unsafe { libc::killpg(group, 0) == 0 }
}

fn foreground_process_group(master: &SharedMasterPty) -> Option<libc::pid_t> {
    master.lock().ok()?.process_group_leader()
}

/// Bound a size a client asked for, on both `CreateSession` and `Resize`.
///
/// The `.max(1)` that used to be spelled out here is now part of
/// [`bounded_screen_dimensions`] and is a `.max(2)`: a client can put `rows: 1`
/// on the wire, and a pane the daemon then runs at one row is a pane the
/// client's own emulator overflows on (A13). Clamping here as well as on the
/// client keeps the `TIOCSWINSZ` the child sees and the screen the client parses
/// at the same size, which is the property that makes a redraw come out right.
fn bounded_pty_dimensions(rows: u16, cols: u16) -> (u16, u16) {
    bounded_screen_dimensions(rows, cols)
}

/// A pane's current foreground process, read with **no pane lock held** (R9).
///
/// This takes the master lock for one `tcgetpgrp` and then reads
/// `/proc/<pid>/cmdline`, and a filesystem read is not bounded in time: a
/// foreground process wedged in uninterruptible sleep on a hung NFS or FUSE
/// mount blocks it indefinitely. Holding the `PaneState` mutex across that — as
/// this used to, being a `&self` method — froze far more than the pane, because
/// [`ServerState::remove_client`] and [`ServerState::session_infos`] take pane
/// locks *while holding the server mutex*: the next `ListSessions` blocked on
/// the server lock and every other connection's `Input`, `Resize` and `Attach`
/// blocked behind it. One wedged pane froze all 256.
///
/// The lock *order* is unchanged and remains acyclic (server -> pane -> master,
/// with the replay gates as leaves). What this establishes is a lock *duration*
/// invariant: no server lock and no pane lock is held across a filesystem read,
/// a `wait`, a sleep, or any other unbounded operation. The master lock is
/// released here before the `/proc` read for the same reason — `resize` takes it
/// under the pane lock.
fn current_foreground_process(
    master: &SharedMasterPty,
    child_pid: Option<u32>,
) -> ForegroundProcessInfo {
    // The guard's scope ends with this statement, so the master lock is not held
    // across the read below.
    let foreground_pid = master
        .lock()
        .ok()
        .and_then(|master| master.process_group_leader())
        .and_then(|pid| u32::try_from(pid).ok());
    let command = foreground_pid.and_then(command_line_for_pid);
    ForegroundProcessInfo {
        root_pid: child_pid,
        foreground_pid,
        command,
    }
}

fn command_line_for_pid(pid: u32) -> Option<String> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    command_line_from_cmdline_bytes(&bytes)
}

fn command_line_from_cmdline_bytes(bytes: &[u8]) -> Option<String> {
    let mut args = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).to_string())
        .collect::<Vec<_>>();
    if args.is_empty() {
        return None;
    }

    if let Some(program) = Path::new(&args[0])
        .file_name()
        .and_then(|name| name.to_str())
    {
        args[0] = program.to_string();
    }

    Some(
        args.into_iter()
            .map(display_arg)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Hand one chunk of input to a pane's writer thread.
///
/// Never blocks, by construction: this runs on the connection's reader thread,
/// and blocking it is what produced the client-side hang (A2). A full queue
/// means the pane's child has stopped reading its stdin and is far enough behind
/// that `PTY_INPUT_QUEUE_CAPACITY` chunks are already waiting; the input is
/// refused with an error the client renders into that pane, rather than dropped
/// in silence (indistinguishable from a broken terminal) or held (a deadlock).
///
/// The queue is bounded twice over: in messages by `PTY_INPUT_QUEUE_CAPACITY`,
/// and in bytes by `PTY_INPUT_QUEUE_MAX_BYTES`, because one message may carry a
/// whole 16 MiB protocol frame . A chunk arriving at an empty queue is
/// always admitted, so a large paste into a pane that *is* reading is never
/// refused for its size.
fn write_pty_input(writer: &SharedPtyInput, bytes: &[u8]) -> Result<(), Rejection> {
    let queued = writer.queued_bytes.load(Ordering::Acquire);
    if queued > 0 && queued.saturating_add(bytes.len()) > PTY_INPUT_QUEUE_MAX_BYTES {
        return Err(Rejection::new(
            RejectCode::InputRefused,
            io::ErrorKind::WouldBlock,
            "input dropped: the program in this pane is not reading its input",
        ));
    }
    writer.queued_bytes.fetch_add(bytes.len(), Ordering::AcqRel);
    match writer.sender.try_send(bytes.to_vec()) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Nothing took the chunk, so nothing will subtract it.
            writer.queued_bytes.fetch_sub(bytes.len(), Ordering::AcqRel);
            Err(pty_input_rejection(error))
        }
    }
}

fn pty_input_rejection(error: mpsc::TrySendError<Vec<u8>>) -> Rejection {
    match error {
        mpsc::TrySendError::Full(_) => Rejection::new(
            RejectCode::InputRefused,
            io::ErrorKind::WouldBlock,
            "input dropped: the program in this pane is not reading its input",
        ),
        mpsc::TrySendError::Disconnected(_) => Rejection::new(
            RejectCode::PaneOperationFailed,
            io::ErrorKind::BrokenPipe,
            "the pane is no longer accepting input",
        ),
    }
}

fn pane_title(shell: &str, launch: &LaunchSpec) -> String {
    match launch {
        LaunchSpec::Shell => shell.to_string(),
        LaunchSpec::Command(command) => command.clone(),
    }
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("server lock poisoned")
}

/// [`lock_error`] for the paths that report through a [`Rejection`]. A poisoned
/// server lock is nobody's fault in particular, so it carries no specific code.
fn pane_lock_rejection<T>(_: std::sync::PoisonError<T>) -> Rejection {
    Rejection::new(
        RejectCode::Unspecified,
        io::ErrorKind::Other,
        "server lock poisoned",
    )
}

fn error_to_io(error: anyhow::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::*;

    /// Input for the A2 flood test: complete lines, because a terminal in
    /// canonical mode *discards* an over-long line rather than blocking on it —
    /// only whole lines nobody reads actually fill the buffer and make a write
    /// to the master block, which is the condition the deadlock needs.
    fn flood_chunk() -> Vec<u8> {
        let mut chunk = Vec::with_capacity(4 * 1024);
        while chunk.len() < 4 * 1024 {
            chunk.extend_from_slice(&[b'x'; 63]);
            chunk.push(b'\n');
        }
        chunk
    }

    /// What the drain thread in the A2 flood test saw.
    enum FloodObservation {
        InputRefused,
        Sessions(Vec<SessionInfo>),
    }

    /// Deadlines for the A2 flood test. Both exist so the deadlock it guards
    /// against fails the test instead of hanging the suite.
    const FLOOD_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
    const FLOOD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

    /// The instance token a test client presents unless it is deliberately
    /// impersonating a second `mult` installation.
    const TEST_INSTANCE: InstanceId = InstanceId(0x7465_7374);
    const OTHER_INSTANCE: InstanceId = InstanceId(0x6f74_6865);

    #[test]
    fn exit_info_preserves_child_exit_code() {
        let info = exit_info(ExitStatus::with_exit_code(7));

        assert_eq!(info.code, 7);
        assert_eq!(info.signal, None);
    }

    #[test]
    fn exit_info_preserves_child_signal() {
        let info = exit_info(ExitStatus::with_signal("SIGTERM"));

        assert_eq!(info.code, 1);
        assert_eq!(info.signal.as_deref(), Some("SIGTERM"));
    }

    #[test]
    fn restrict_socket_permissions_sets_user_only_mode() {
        let path = unique_socket_path();
        let _listener = UnixListener::bind(&path).expect("bind test socket");

        restrict_socket_permissions(&path).expect("restrict socket permissions");

        let mode = fs::metadata(&path)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_file(&path).expect("remove test socket");
    }

    #[test]
    fn the_daemon_verifies_peer_credentials_through_the_shared_check() {
        // The accept/reject rule itself is tested in `mult_protocol::peer`; what
        // matters here is that this binary is wired to it at all, since the
        // check used to be a private copy that could rot independently.
        let (client, _server) = UnixStream::pair().expect("create socket pair");

        verify_peer_is_self(&client, "test client").expect("same uid peer is accepted");
    }

    #[test]
    fn bind_socket_path_creates_missing_parent_with_user_only_mode() {
        let dir = unique_socket_dir();
        let path = dir.join("mult.sock");

        bind_socket_path(&path).expect("prepare socket path");

        let mode = fs::metadata(&dir)
            .expect("socket parent metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        fs::remove_dir_all(&dir).expect("remove socket dir");
    }

    #[test]
    fn server_rejects_incompatible_client_protocol_version() {
        let (mut client_stream, server_stream) = UnixStream::pair().expect("create socket pair");
        client_stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let server = Arc::new(Mutex::new(ServerState::default()));
        let server_thread = thread::spawn(move || handle_client(server_stream, server));

        write_message(
            &mut client_stream,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION + 1,
                instance: TEST_INSTANCE,
            },
        )
        .expect("write incompatible hello");

        let message: ServerMessage = read_message(&mut client_stream).expect("read error");
        assert!(matches!(
            message,
            ServerMessage::Error {
                code: RejectCode::ProtocolMismatch,
                ..
            }
        ));
        server_thread
            .join()
            .expect("server thread should not panic")
            .expect("server handles incompatible hello");
    }

    #[test]
    fn server_rejects_non_hello_first_message() {
        let (mut client_stream, server_stream) = UnixStream::pair().expect("create socket pair");
        client_stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let server = Arc::new(Mutex::new(ServerState::default()));
        let server_thread = thread::spawn(move || handle_client(server_stream, server));

        write_message(&mut client_stream, &ClientMessage::ListSessions)
            .expect("write non-hello message");

        let message: ServerMessage = read_message(&mut client_stream).expect("read error");
        assert!(matches!(
            message,
            ServerMessage::Error {
                code: RejectCode::HelloRequired,
                ..
            }
        ));
        server_thread
            .join()
            .expect("server thread should not panic")
            .expect("server rejects non-hello first message");
    }

    /// Ceiling for "the server eventually answered". Every wait below returns as
    /// soon as the message it wants arrives, so this only costs wall-clock time
    /// on a genuine failure.
    const DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);

    /// Whether this environment can allocate a PTY, and so whether the
    /// dispatch tests that create a pane can run here.
    ///
    /// These tests spawn a real child on a real PTY on purpose — the dispatch
    /// loop's job *is* to move bytes between a socket and a pty master, and a
    /// fake would test the fake. They cannot be made hermetic: a Nix build
    /// sandbox has a `/dev/ptmx` symlink but no `devpts` mounted behind it, so
    /// `openpty` fails with `ENOENT` before any of `mult`'s own code runs.
    ///
    /// So they use the same explicit, documented opt-out the integration suite
    /// uses — `MULT_SKIP_PTY_INTEGRATION`, set by `flake.nix` and by nothing
    /// else. Explicit is the whole point (G1): a failure to allocate a PTY is
    /// never quietly green, it is green only where the operator has said this
    /// environment has no PTYs, and everywhere else — every developer machine
    /// and both CI runners — the tests run.
    fn pty_backed_tests_are_opted_out() -> bool {
        std::env::var_os("MULT_SKIP_PTY_INTEGRATION").is_some_and(|value| !value.is_empty())
    }

    /// A client connection driven end to end against a real dispatch loop: a
    /// socket pair, a `handle_client` thread and a completed protocol hello.
    struct DispatchSession {
        stream: UnixStream,
        thread: Option<thread::JoinHandle<io::Result<()>>>,
    }

    impl DispatchSession {
        fn start() -> Self {
            Self::start_on(
                Arc::new(Mutex::new(ServerState::default())),
                TEST_INSTANCE,
                None,
            )
        }

        /// A connection on `server` presenting `instance`. Two sessions sharing
        /// a `server` with *different* instances are two `mult` installations
        /// talking to one daemon, which is what A3 is about.
        fn start_on(
            server: SharedServer,
            instance: InstanceId,
            write_timeout: Option<Duration>,
        ) -> Self {
            let (mut stream, server_stream) = UnixStream::pair().expect("create socket pair");
            stream
                .set_read_timeout(Some(DISPATCH_TIMEOUT))
                .expect("set read timeout");
            // A test that deliberately floods the daemon must fail rather than
            // block if the daemon stops reading (A2), so its writes get a
            // deadline too.
            stream
                .set_write_timeout(write_timeout)
                .expect("set write timeout");
            let thread = thread::spawn(move || handle_client(server_stream, server));

            write_message(
                &mut stream,
                &ClientMessage::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    instance,
                },
            )
            .expect("write hello");
            let mut session = Self {
                stream,
                thread: Some(thread),
            };
            session.expect(
                |message| matches!(message, ServerMessage::Hello { .. }),
                "protocol hello",
            );
            session
        }

        fn send(&mut self, message: ClientMessage) {
            write_message(&mut self.stream, &message).expect("write client message");
        }

        /// Read until `matches` accepts a message. An unexpected
        /// `ServerMessage::Error` fails the test immediately, which is what makes
        /// "the server did not reject this" an assertion rather than a hope.
        fn expect(
            &mut self,
            mut matches: impl FnMut(&ServerMessage) -> bool,
            what: &str,
        ) -> ServerMessage {
            loop {
                let message = read_message::<ServerMessage>(&mut self.stream)
                    .unwrap_or_else(|error| panic!("waiting for {what}: {error}"));
                if matches(&message) {
                    return message;
                }
                if let ServerMessage::Error { message, .. } = &message {
                    panic!("server rejected the request while waiting for {what}: {message}");
                }
            }
        }

        /// Create a `cat` pane and attach to it. `cat` keeps the pane alive and
        /// echoes input back, so round trips do not depend on shell behaviour.
        fn create_and_attach(&mut self, session: SessionId, rows: u16, cols: u16) -> SessionId {
            self.create_and_attach_command(session, "cat", rows, cols)
        }

        /// The same, for a pane that must run something other than `cat` — a
        /// program that never reads its stdin, say.
        fn create_and_attach_command(
            &mut self,
            session: SessionId,
            command: &str,
            rows: u16,
            cols: u16,
        ) -> SessionId {
            self.send(ClientMessage::CreateSession {
                requested_id: Some(session),
                name: "dispatch-test".to_string(),
                cwd: None,
                env: BTreeMap::new(),
                launch: LaunchSpec::Command(command.to_string()),
                rows,
                cols,
            });
            let created = self.expect(
                |message| matches!(message, ServerMessage::Sessions(sessions) if !sessions.is_empty()),
                "created session",
            );
            let ServerMessage::Sessions(sessions) = created else {
                unreachable!("matched above");
            };
            assert_eq!(sessions[0].id, session);

            self.send(ClientMessage::Attach {
                session,
                rows,
                cols,
            });
            let attached = self.expect(
                |message| matches!(message, ServerMessage::Attached { session: attached, .. } if *attached == session),
                "attach confirmation",
            );
            let ServerMessage::Attached { panes, .. } = attached else {
                unreachable!("matched above");
            };
            // Every attach replays scrollback, even when the pane is silent.
            self.expect(
                |message| matches!(message, ServerMessage::PtyScrollback { .. }),
                "scrollback replay",
            );
            panes[0].id
        }

        fn expect_echo(&mut self, pane: SessionId, marker: &str) {
            self.send(ClientMessage::Input {
                pane,
                bytes: format!("{marker}\n").into_bytes(),
            });
            let mut seen = Vec::new();
            self.expect(
                |message| match message {
                    ServerMessage::PtyOutput { bytes, .. } => {
                        seen.extend_from_slice(bytes);
                        String::from_utf8_lossy(&seen).contains(marker)
                    }
                    _ => false,
                },
                "echoed PTY output",
            );
        }

        fn stop(&mut self, pane: SessionId) {
            self.send(ClientMessage::Stop { pane });
            self.send(ClientMessage::ListSessions);
            self.expect(
                |message| matches!(message, ServerMessage::Sessions(sessions) if sessions.iter().all(|session| session.id != pane)),
                "session list without the stopped pane",
            );
        }

        fn finish(mut self) {
            self.send(ClientMessage::Detach);
            self.thread
                .take()
                .expect("dispatch thread")
                .join()
                .expect("dispatch thread should not panic")
                .expect("dispatch loop should end cleanly");
        }
    }

    impl Drop for DispatchSession {
        fn drop(&mut self) {
            // Shut the socket down so a healthy dispatch thread ends, but do
            // *not* join it: a failing test may have left the daemon wedged in
            // exactly the blocking write these tests exist to rule out, and
            // joining it there would turn a failure into a hung CI run. The
            // thread is detached instead; the process reaps it on exit.
            let _ = self.stream.shutdown(Shutdown::Both);
            self.thread.take();
        }
    }

    #[test]
    fn client_session_create_attach_input_resize_stop_round_trip() {
        if pty_backed_tests_are_opted_out() {
            return;
        }

        let mut session = DispatchSession::start();
        let pane = session.create_and_attach(SessionId(4_001), 24, 80);

        session.expect_echo(pane, "round-trip-marker");

        session.send(ClientMessage::Resize {
            pane,
            rows: 12,
            cols: 40,
        });
        // The pane is still usable after the resize, which is the observable
        // half of "resize was applied and nothing was torn down".
        session.expect_echo(pane, "after-resize-marker");

        session.stop(pane);
        session.finish();
    }

    #[test]
    fn resize_beyond_bounds_is_clamped_not_rejected() {
        if pty_backed_tests_are_opted_out() {
            return;
        }

        let mut session = DispatchSession::start();
        let pane = session.create_and_attach(SessionId(4_002), 24, 80);

        session.send(ClientMessage::Resize {
            pane,
            rows: u16::MAX,
            cols: u16::MAX,
        });
        // An out-of-range resize is clamped, not answered with an error and not
        // grounds for dropping the connection: the pane keeps working, and a
        // re-attach reports the clamped geometry.
        session.expect_echo(pane, "clamped-marker");
        session.send(ClientMessage::Attach {
            session: SessionId(4_002),
            rows: u16::MAX,
            cols: u16::MAX,
        });
        let attached = session.expect(
            |message| matches!(message, ServerMessage::Attached { .. }),
            "attach after an oversized resize",
        );
        let ServerMessage::Attached { panes, .. } = attached else {
            unreachable!("matched above");
        };
        assert!(panes[0].rows > 0 && panes[0].rows <= mult_protocol::MAX_SCREEN_ROWS);
        assert!(panes[0].cols > 0 && panes[0].cols <= mult_protocol::MAX_SCREEN_COLS);
        assert!(
            usize::from(panes[0].rows) * usize::from(panes[0].cols)
                <= mult_protocol::MAX_SCREEN_CELLS
        );

        session.stop(pane);
        session.finish();
    }

    /// A13, daemon side. Nothing stops a client — or a rogue peer — putting a
    /// one-row pane on the wire, and a pane the daemon runs at one row is a pane
    /// the client's emulator overflows on. The floor is applied to `Resize` and
    /// to `CreateSession` alike, so the two ends never disagree about the size a
    /// child is drawing for.
    #[test]
    fn a_one_row_resize_is_raised_to_the_emulator_floor() {
        if pty_backed_tests_are_opted_out() {
            return;
        }

        let mut session = DispatchSession::start();
        let pane = session.create_and_attach(SessionId(4_009), 1, 1);

        session.send(ClientMessage::Resize {
            pane,
            rows: 1,
            cols: 1,
        });
        session.expect_echo(pane, "tiny-pane-marker");

        session.send(ClientMessage::Attach {
            session: SessionId(4_009),
            rows: 1,
            cols: 1,
        });
        let attached = session.expect(
            |message| matches!(message, ServerMessage::Attached { .. }),
            "attach after a one-row resize",
        );
        let ServerMessage::Attached { panes, .. } = attached else {
            unreachable!("matched above");
        };
        assert!(panes[0].rows >= mult_protocol::MIN_SCREEN_ROWS);
        assert!(panes[0].cols >= mult_protocol::MIN_SCREEN_COLS);

        session.stop(pane);
        session.finish();
    }

    #[test]
    fn stop_on_unknown_pane_returns_error_without_killing_others() {
        if pty_backed_tests_are_opted_out() {
            return;
        }

        let mut session = DispatchSession::start();
        let pane = session.create_and_attach(SessionId(4_003), 24, 80);
        session.expect_echo(pane, "before-bad-stop");

        // A stale pane id used to propagate out of the dispatch loop and take
        // the whole connection — and every other pane on it — down with it.
        session.send(ClientMessage::Stop {
            pane: SessionId(999_001),
        });
        session.expect(
            |message| {
                matches!(
                    message,
                    ServerMessage::Error {
                        code: RejectCode::UnknownSession,
                        ..
                    }
                )
            },
            "error for the unknown pane",
        );

        session.expect_echo(pane, "after-bad-stop");
        session.stop(pane);
        session.finish();
    }

    /// A2: the deadlock, and that refusing input is loud.
    ///
    /// A pane whose child never reads its stdin fills the PTY's input buffer
    /// permanently. Writing to that master from the connection's reader thread
    /// — as this daemon used to — stops the daemon reading the socket, the
    /// socket buffer fills, and the client's render thread blocks in its own
    /// `write_all`: both ends hang, forever, over a paste.
    ///
    /// Both halves of the hang are made to *fail* rather than hang: the client
    /// socket has a write deadline (so the flood errors out instead of blocking)
    /// and the check afterwards waits on a channel with a deadline. A separate
    /// drain thread reads the connection throughout, because a client that stops
    /// reading is a slow client and is legitimately disconnected.
    #[test]
    fn a_pane_that_never_reads_input_cannot_wedge_the_dispatch_loop() {
        if pty_backed_tests_are_opted_out() {
            return;
        }

        let mut session = DispatchSession::start_on(
            Arc::new(Mutex::new(ServerState::default())),
            TEST_INSTANCE,
            Some(FLOOD_WRITE_TIMEOUT),
        );
        let pane = session.create_and_attach_command(SessionId(4_010), "sleep 30", 24, 80);

        let mut drain = session.stream.try_clone().expect("clone the client socket");
        let (events, observed) = mpsc::channel();
        let drainer = thread::spawn(move || {
            while let Ok(message) = read_message::<ServerMessage>(&mut drain) {
                match message {
                    ServerMessage::Error {
                        code: RejectCode::InputRefused,
                        ..
                    } => {
                        let _ = events.send(FloodObservation::InputRefused);
                    }
                    ServerMessage::Sessions(sessions) => {
                        let _ = events.send(FloodObservation::Sessions(sessions));
                    }
                    _ => {}
                }
            }
        });

        // Far more than the few KiB a PTY buffers, in far more messages than the
        // pane's input queue holds.
        let chunk = flood_chunk();
        for index in 0..80 {
            write_message(
                &mut session.stream,
                &ClientMessage::Input {
                    pane,
                    bytes: chunk.clone(),
                },
            )
            .unwrap_or_else(|error| {
                panic!(
                    "input {index} blocked: a full PTY must never stall the daemon's socket reader: {error}"
                )
            });
        }

        // The daemon is still serving this connection: it processes an unrelated
        // request while the pane's input queue is backed up.
        session.send(ClientMessage::Stop { pane });
        session.send(ClientMessage::ListSessions);

        let mut refused = false;
        let deadline = Instant::now() + FLOOD_RESPONSE_TIMEOUT;
        let sessions = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match observed.recv_timeout(remaining) {
                Ok(FloodObservation::InputRefused) => refused = true,
                Ok(FloodObservation::Sessions(sessions)) => break sessions,
                Err(error) => {
                    panic!("the daemon stopped answering while a pane was backed up: {error}")
                }
            }
        };

        assert!(
            sessions.iter().all(|session| session.id != pane),
            "the stopped pane should be gone from the session list"
        );
        assert!(
            refused,
            "input dropped by a full pane queue must be reported, never swallowed"
        );

        session.finish();
        drainer.join().expect("drain thread should not panic");
    }

    /// A3 + C12: two `mult` installations, one daemon, the same session number.
    /// The second must get its own pane and must not evict the first — which is
    /// exactly what a global session map did, handing over the live PTY stream
    /// of whatever the first instance was running.
    #[test]
    fn a_second_instance_cannot_take_over_the_first_instances_session() {
        if pty_backed_tests_are_opted_out() {
            return;
        }

        let server = Arc::new(Mutex::new(ServerState::default()));
        let mut first = DispatchSession::start_on(Arc::clone(&server), TEST_INSTANCE, None);
        let mut second = DispatchSession::start_on(Arc::clone(&server), OTHER_INSTANCE, None);

        let first_pane = first.create_and_attach(SessionId(4_020), 24, 80);
        first.expect_echo(first_pane, "first-instance-marker");

        // Same requested id, different instance: a separate session entirely.
        let second_pane = second.create_and_attach(SessionId(4_020), 24, 80);
        second.expect_echo(second_pane, "second-instance-marker");

        // The first connection is untouched: it still owns its pane and still
        // echoes. `expect` fails the test on any error, so the takeover notice
        // the old code sent here would be caught even if the pane still worked.
        first.expect_echo(first_pane, "first-instance-still-attached");

        // And neither instance can even see the other's session.
        for (session, label) in [(&mut first, "first"), (&mut second, "second")] {
            session.send(ClientMessage::ListSessions);
            let listed = session.expect(
                |message| matches!(message, ServerMessage::Sessions(_)),
                "a session list",
            );
            let ServerMessage::Sessions(sessions) = listed else {
                unreachable!("matched above");
            };
            assert_eq!(sessions.len(), 1, "{label} instance sees a foreign session");
        }

        first.stop(first_pane);
        second.stop(second_pane);
        first.finish();
        second.finish();
    }

    /// A3, the other direction: the same instance reconnecting *must* get its
    /// own pane back, history and all. That is what the daemon exists for, and
    /// it is the behaviour the namespace must not cost us.
    #[test]
    fn the_same_instance_reclaims_its_session_after_reconnecting() {
        if pty_backed_tests_are_opted_out() {
            return;
        }

        let server = Arc::new(Mutex::new(ServerState::default()));
        let session_id = SessionId(4_021);
        let mut first = DispatchSession::start_on(Arc::clone(&server), TEST_INSTANCE, None);
        let pane = first.create_and_attach(session_id, 24, 80);
        first.expect_echo(pane, "survives-the-reconnect");
        first.finish();

        // A fresh connection presenting the same token, as a restarted client
        // does: the session is found, re-attached, and replayed.
        let mut reconnected = DispatchSession::start_on(Arc::clone(&server), TEST_INSTANCE, None);
        reconnected.send(ClientMessage::Attach {
            session: session_id,
            rows: 24,
            cols: 80,
        });
        reconnected.expect(
            |message| matches!(message, ServerMessage::Attached { .. }),
            "re-attach confirmation for the reclaimed session",
        );
        let mut replayed = Vec::new();
        reconnected.expect(
            |message| match message {
                ServerMessage::PtyScrollback { bytes, .. } => {
                    replayed.extend_from_slice(bytes);
                    String::from_utf8_lossy(&replayed).contains("survives-the-reconnect")
                }
                _ => false,
            },
            "replayed history of the reclaimed session",
        );

        reconnected.stop(pane);
        reconnected.finish();
    }

    /// A10: the session cap. Each session pins a PTY, two threads and up to
    /// `RAW_HISTORY_MAX_BYTES`, so a `CreateSession` loop used to be able to
    /// take out every pane the user already had.
    #[test]
    fn the_session_cap_is_reported_as_an_error_not_a_panic() {
        let mut server = ServerState::default();
        for index in 0..MAX_SESSIONS {
            server
                .reserve_session_id(TEST_INSTANCE, Some(SessionId(index as u64 + 1)))
                .expect("reserve up to the cap");
        }

        let error = server
            .reserve_session_id(TEST_INSTANCE, Some(SessionId(999_999)))
            .expect_err("the session over the cap must be refused");

        // The cap is reported by code, not by the wording of the message.
        assert_eq!(error.code, RejectCode::SessionLimit);
        assert_eq!(error.error.kind(), io::ErrorKind::WouldBlock);
        // Another instance does not get a fresh allowance: the cap is on the
        // daemon's resources, which are shared.
        assert!(server
            .reserve_session_id(OTHER_INSTANCE, Some(SessionId(1)))
            .is_err());
    }

    /// A10: the connection cap, and that an over-cap client is *told* rather
    /// than left hanging on a socket nobody is reading.
    #[test]
    fn a_connection_over_the_client_cap_is_refused_with_an_error() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        {
            let mut locked = server.lock().expect("server lock");
            for _ in 0..MAX_CLIENTS {
                locked.register_client().expect("register up to the cap");
            }
        }

        let (mut client_stream, server_stream) = UnixStream::pair().expect("create socket pair");
        client_stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");
        let thread = thread::spawn(move || handle_client(server_stream, server));

        let message: ServerMessage =
            read_message(&mut client_stream).expect("the refusal must be sent, not dropped");
        assert!(matches!(
            message,
            ServerMessage::Error {
                code: RejectCode::ConnectionLimit,
                ..
            }
        ));
        thread
            .join()
            .expect("the refusal must not panic the daemon")
            .expect("refusing a client is not a server error");
    }

    /// A10: an established connection keeps a deadline. Silence for longer than
    /// `CLIENT_IDLE_TIMEOUT` is reported as idleness — a connection to reclaim —
    /// rather than as an I/O failure or, as before, not at all.
    #[test]
    fn a_read_deadline_expiring_is_reported_as_an_idle_connection() {
        let (_peer, mut server_stream) = UnixStream::pair().expect("create socket pair");
        server_stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set read timeout");

        let read = read_client_message(&mut server_stream).expect("a deadline is not a failure");

        assert!(matches!(read, ClientRead::Idle));
    }

    #[test]
    fn a_hello_without_an_instance_token_is_refused() {
        let (mut client_stream, server_stream) = UnixStream::pair().expect("create socket pair");
        client_stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");
        let server = Arc::new(Mutex::new(ServerState::default()));
        let thread = thread::spawn(move || handle_client(server_stream, server));

        write_message(
            &mut client_stream,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                instance: InstanceId::UNSET,
            },
        )
        .expect("write hello without a token");

        let message: ServerMessage = read_message(&mut client_stream).expect("read error");
        assert!(matches!(
            message,
            ServerMessage::Error {
                code: RejectCode::InstanceTokenRequired,
                ..
            }
        ));
        thread
            .join()
            .expect("server thread should not panic")
            .expect("refusing an unnamespaced client is not a server error");
    }

    #[test]
    fn pty_dimensions_are_bounded_for_server_allocations() {
        let (rows, cols) = bounded_pty_dimensions(u16::MAX, u16::MAX);

        assert!(usize::from(rows) * usize::from(cols) <= mult_protocol::MAX_SCREEN_CELLS);
        assert!(rows > 0);
        assert!(cols > 0);
    }

    #[test]
    fn raw_history_is_capped_and_keeps_the_newest_bytes() {
        let mut pane = test_pane_state();
        let overflow = RAW_HISTORY_CHUNK_BYTES + 10;
        pane.raw_history.append(&vec![b'a'; RAW_HISTORY_MAX_BYTES]);
        pane.raw_history.append(&vec![b'b'; overflow]);

        assert_eq!(pane.raw_history.len(), RAW_HISTORY_MAX_BYTES);
        let replayed = concatenated_history(&pane.raw_history);
        assert_eq!(replayed.len(), RAW_HISTORY_MAX_BYTES);
        assert_eq!(
            replayed[RAW_HISTORY_MAX_BYTES - overflow..],
            vec![b'b'; overflow],
            "the newest bytes must survive the trim"
        );
        assert!(
            replayed[..RAW_HISTORY_MAX_BYTES - overflow]
                .iter()
                .all(|byte| *byte == b'a'),
            "the surviving prefix must be the tail of the older bytes"
        );
    }

    #[test]
    fn raw_history_trim_advances_past_dropped_bytes_without_moving_retained_ones() {
        // Regression guard for the O(history) trim: a flat `Vec<u8>` with
        // `drain(..overflow)` memmoves every retained byte on every read once
        // the cap is reached. The chunked history must instead leave the bytes
        // it keeps exactly where they are, so their addresses are stable and
        // only the front offset moves.
        let mut history = RawHistory::new();
        history.append(&vec![b'a'; RAW_HISTORY_MAX_BYTES]);
        let before = history.snapshot();
        let front_before = before.chunks().next().expect("front chunk").as_ptr() as usize;
        let second_before = before.chunks().nth(1).expect("second chunk").as_ptr() as usize;

        history.append(b"b");

        assert_eq!(history.len(), RAW_HISTORY_MAX_BYTES);
        let after = history.snapshot();
        assert_eq!(
            after.chunks().next().expect("front chunk").as_ptr() as usize,
            front_before + 1,
            "dropping one byte must only advance into the front chunk"
        );
        assert_eq!(
            after.chunks().nth(1).expect("second chunk").as_ptr() as usize,
            second_before,
            "retained chunks must never be copied or moved by a trim"
        );
    }

    #[test]
    fn session_allocation_skips_reserved_ids() {
        let mut server = ServerState::default();
        server
            .reserve_session_id(TEST_INSTANCE, Some(SessionId(1)))
            .expect("reserve first session");

        assert_eq!(server.allocate_session_id(TEST_INSTANCE), SessionId(2));
    }

    #[test]
    fn duplicate_requested_session_reservation_is_rejected() {
        let mut server = ServerState::default();
        server
            .reserve_session_id(TEST_INSTANCE, Some(SessionId(7)))
            .expect("reserve requested session");

        let error = server
            .reserve_session_id(TEST_INSTANCE, Some(SessionId(7)))
            .expect_err("duplicate reservation should fail");

        assert_eq!(error.code, RejectCode::SessionCreateFailed);
        assert_eq!(error.error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn reservation_is_released_when_spawn_fails() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let error = match create_session_with_spawner(
            &server,
            TEST_INSTANCE,
            SessionCreateSpec {
                requested_id: Some(SessionId(11)),
                pane: PaneSpawnSpec {
                    name: "broken".to_string(),
                    cwd: None,
                    env: BTreeMap::new(),
                    launch: LaunchSpec::Shell,
                    rows: 24,
                    cols: 80,
                },
            },
            |_key, _spec| Err(io::Error::other("injected spawn failure")),
        ) {
            Ok(_) => panic!("injected spawn failure should fail"),
            Err(error) => error,
        };

        assert_eq!(error.code, RejectCode::SessionCreateFailed);
        assert_eq!(error.error.kind(), io::ErrorKind::Other);
        assert!(server
            .lock()
            .expect("server lock")
            .reserved_sessions
            .is_empty());
    }

    #[test]
    fn bind_socket_path_refuses_to_remove_existing_non_socket_path() {
        let path = unique_socket_path();
        fs::write(&path, "do not remove").expect("write collision file");

        let error = bind_socket_path(&path).expect_err("refuse non-socket collision");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(&path).expect("collision file remains"),
            "do not remove"
        );
        fs::remove_file(&path).expect("remove collision file");
    }

    #[test]
    fn bind_socket_path_removes_stale_socket_file() {
        let path = unique_socket_path();
        let listener = UnixListener::bind(&path).expect("bind stale socket");
        drop(listener);

        bind_socket_path(&path).expect("remove stale socket");

        assert!(!path.exists());
    }

    #[test]
    fn command_line_from_cmdline_bytes_formats_process_args() {
        assert_eq!(
            command_line_from_cmdline_bytes(b"/usr/bin/cargo\0test\0--\0space value\0"),
            Some("cargo test -- 'space value'".to_string())
        );
        assert_eq!(command_line_from_cmdline_bytes(b""), None);
    }

    #[test]
    fn broadcast_drops_full_client_without_blocking_the_reader() {
        // Keep the receiver alive so the channel reports Full (a slow client),
        // not Disconnected (a gone client).
        let (sender, _receiver) = mpsc::sync_channel(1);
        let (stream, _peer) = UnixStream::pair().expect("create socket pair");
        let client = ClientHandle {
            id: 1,
            sender,
            stream: Arc::new(stream),
        };
        // Fill the one-slot queue; a blocking send would now wedge the reader.
        client
            .sender
            .try_send(ServerMessage::PtyOutput {
                pane: SessionId(1),
                bytes: Vec::new(),
            })
            .expect("prime the client queue");

        // Returns promptly (the test would hang on a blocking send) and reports
        // the slow client for eviction.
        let dropped = broadcast_pty_output(
            SessionId(1),
            b"more".to_vec(),
            std::slice::from_ref(&open_pane_client(client)),
        );

        assert_eq!(dropped, vec![1]);
    }

    #[test]
    fn broadcast_keeps_client_that_drains_its_queue() {
        let (sender, receiver) = mpsc::sync_channel(CLIENT_QUEUE_CAPACITY);
        let (stream, _peer) = UnixStream::pair().expect("create socket pair");
        let client = ClientHandle {
            id: 7,
            sender,
            stream: Arc::new(stream),
        };

        let dropped = broadcast_pty_output(
            SessionId(3),
            b"data".to_vec(),
            std::slice::from_ref(&open_pane_client(client)),
        );

        assert!(dropped.is_empty());
        assert!(matches!(
            receiver.recv(),
            Ok(ServerMessage::PtyOutput { pane, bytes }) if pane == SessionId(3) && bytes == b"data"
        ));
    }

    #[test]
    fn attaching_a_new_client_takes_over_from_the_previous_one() {
        let mut pane = test_pane_state();

        assert!(pane
            .attach_client(open_pane_client(test_client(1)))
            .is_empty());
        assert_eq!(pane.clients.len(), 1);
        assert_eq!(pane.clients[0].id(), 1);

        // A second client takes over: the previous one is evicted and handed
        // back so it can be told, not dropped in silence.
        let evicted = pane.attach_client(open_pane_client(test_client(2)));
        assert_eq!(
            evicted.iter().map(PaneClient::id).collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(pane.clients.len(), 1);
        assert_eq!(pane.clients[0].id(), 2);

        // Re-attaching the same id is idempotent (no duplicate entry, nothing
        // evicted).
        assert!(pane
            .attach_client(open_pane_client(test_client(2)))
            .is_empty());
        assert_eq!(pane.clients.len(), 1);
        assert_eq!(pane.clients[0].id(), 2);
    }

    #[test]
    fn evicted_clients_are_retired_with_a_pane_exit_before_removal() {
        let (sender, receiver) = mpsc::sync_channel(CLIENT_QUEUE_CAPACITY);
        let (stream, mut peer) = UnixStream::pair().expect("create socket pair");
        let evicted = ClientHandle {
            id: 3,
            sender,
            stream: Arc::new(stream),
        };

        let evicted = open_pane_client(evicted);
        notify_evicted_clients(std::slice::from_ref(&evicted), SessionId(12));

        assert!(matches!(
            receiver.recv().expect("takeover notice"),
            ServerMessage::Error {
                code: RejectCode::SessionBusy,
                ..
            }
        ));
        assert!(matches!(
            receiver.recv().expect("pane retirement"),
            ServerMessage::PaneExited { pane, .. } if pane == SessionId(12)
        ));
        // A client that took the notice keeps its connection: its other panes
        // have nothing to do with this takeover.
        peer.set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set read timeout");
        let mut probe = [0; 1];
        assert!(
            matches!(peer.read(&mut probe), Err(error) if error.kind() == io::ErrorKind::WouldBlock
                || error.kind() == io::ErrorKind::TimedOut),
            "a notified client must not be disconnected"
        );
    }

    #[test]
    fn evicted_client_that_cannot_be_notified_is_disconnected() {
        // A one-slot queue that is already full stands in for a client whose
        // writer is wedged: it cannot be told, so it must be reaped instead.
        let (sender, _receiver) = mpsc::sync_channel(1);
        let (stream, mut peer) = UnixStream::pair().expect("create socket pair");
        let evicted = ClientHandle {
            id: 4,
            sender,
            stream: Arc::new(stream),
        };
        evicted
            .sender
            .try_send(ServerMessage::Sessions(Vec::new()))
            .expect("prime the client queue");

        let evicted = open_pane_client(evicted);
        notify_evicted_clients(std::slice::from_ref(&evicted), SessionId(1));

        let mut probe = [0; 1];
        assert_eq!(
            peer.read(&mut probe).expect("read from disconnected peer"),
            0,
            "an unreachable evicted client must be disconnected"
        );
    }

    #[test]
    fn scrollback_replay_is_chunked_without_gaps_or_overlap() {
        for size in [
            RAW_HISTORY_CHUNK_BYTES - 1,
            RAW_HISTORY_CHUNK_BYTES,
            RAW_HISTORY_CHUNK_BYTES + 1,
            3 * RAW_HISTORY_CHUNK_BYTES + 7,
        ] {
            let source = history_bytes(size);
            let mut history = RawHistory::new();
            // Odd-sized writes, so PTY read boundaries never line up with the
            // chunk boundaries the replay actually sends.
            for write in source.chunks(3_997) {
                history.append(write);
            }
            assert_eq!(history.len(), size);

            let (client, receiver, _peer) = queued_test_client(CLIENT_QUEUE_CAPACITY);
            send_pty_scrollback(&client, SessionId(5), &history.snapshot());

            let mut replayed = Vec::new();
            let mut messages = 0;
            while let Ok(message) = receiver.try_recv() {
                match message {
                    ServerMessage::PtyScrollback { pane, bytes } => {
                        assert_eq!(pane, SessionId(5));
                        assert!(
                            bytes.len() <= RAW_HISTORY_CHUNK_BYTES,
                            "replay chunk of {} bytes exceeds the chunk size",
                            bytes.len()
                        );
                        replayed.extend_from_slice(&bytes);
                        messages += 1;
                    }
                    other => panic!("unexpected message during replay: {other:?}"),
                }
            }

            assert_eq!(
                messages,
                size.div_ceil(RAW_HISTORY_CHUNK_BYTES),
                "history of {size} bytes should replay in whole chunks"
            );
            assert_eq!(
                replayed, source,
                "replay of {size} bytes must reproduce the history exactly"
            );
        }
    }

    #[test]
    fn empty_scrollback_still_sends_one_terminator_message() {
        let history = RawHistory::new();
        let (client, receiver, _peer) = queued_test_client(CLIENT_QUEUE_CAPACITY);

        send_pty_scrollback(&client, SessionId(2), &history.snapshot());

        assert!(matches!(
            receiver.try_recv().expect("terminator message"),
            ServerMessage::PtyScrollback { pane, bytes } if pane == SessionId(2) && bytes.is_empty()
        ));
        assert!(receiver.try_recv().is_err(), "exactly one message expected");
    }

    #[test]
    fn scrollback_replay_waits_for_a_backlogged_client_instead_of_evicting_it() {
        // The queue already holds a message and has no free slot: on the live
        // broadcast path that is an eviction. A replay must not evict the client
        // it has just attached, or that client reconnects, re-attaches, and
        // overflows again forever.
        let source = history_bytes(2 * RAW_HISTORY_CHUNK_BYTES);
        let mut history = RawHistory::new();
        history.append(&source);
        let (client, receiver, mut peer) = queued_test_client(1);
        client
            .sender
            .try_send(ServerMessage::Sessions(Vec::new()))
            .expect("prime the client queue");

        // One primed message plus one per replayed chunk; the client socket is
        // kept alive so the connectivity assertion below means what it says.
        let expected = 1 + 2;
        let drainer = thread::spawn(move || {
            let mut received = Vec::new();
            while received.len() < expected {
                match receiver.recv_timeout(SCROLLBACK_SEND_TIMEOUT) {
                    Ok(message) => received.push(message),
                    Err(_) => break,
                }
            }
            received
        });
        send_pty_scrollback(&client, SessionId(8), &history.snapshot());

        let mut replayed = Vec::new();
        for message in drainer.join().expect("drainer thread") {
            if let ServerMessage::PtyScrollback { bytes, .. } = message {
                replayed.extend_from_slice(&bytes);
            }
        }
        assert_eq!(
            replayed, source,
            "a backlogged client still gets the replay"
        );
        let mut probe = [0; 1];
        peer.set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set read timeout");
        assert!(
            matches!(peer.read(&mut probe), Err(error) if error.kind() == io::ErrorKind::WouldBlock
                || error.kind() == io::ErrorKind::TimedOut),
            "the replay must not disconnect the client it is replaying to"
        );
    }

    /// R2: attaching to a *busy* pane.
    ///
    /// The pane's reader thread is typically already blocked on the pane lock
    /// when `Attach` releases it, so before the replay gate it appended a chunk
    /// that was not in the replay snapshot and `try_send`'d it straight to the
    /// client the attach had just registered. The client then fed
    /// `PtyOutput(new)` and `PtyScrollback(old)` into one parser with no reset
    /// and kept a permanently mis-ordered screen.
    ///
    /// The hook runs in exactly that window, and does exactly what the reader
    /// thread does there.
    #[test]
    fn attach_holds_live_output_until_the_scrollback_replay_is_queued() {
        if pty_backed_tests_are_opted_out() {
            return;
        }

        let mut state = test_pane_state();
        state.raw_history.append(b"old-history");
        let pane: SharedPane = Arc::new(Mutex::new(state));
        let (client, receiver, _peer) = queued_test_client(CLIENT_QUEUE_CAPACITY);

        attach_pane(&client, &pane, SessionId(1), 24, 80, || {
            let clients = {
                // Bounded, so an attach that (wrongly) kept the pane lock across
                // its replay fails this test instead of hanging it.
                let mut locked = lock_pane_within(&pane, DISPATCH_TIMEOUT);
                locked.raw_history.append(b"live-output");
                locked.clients.clone()
            };
            assert!(
                broadcast_pty_output(SessionId(1), b"live-output".to_vec(), &clients).is_empty(),
                "the live chunk must be held for the client, not refused"
            );
        })
        .expect("attach the client");

        let mut replayed = Vec::new();
        let mut live = Vec::new();
        while let Ok(message) = receiver.try_recv() {
            match message {
                ServerMessage::PtyScrollback { bytes, .. } => {
                    assert!(
                        live.is_empty(),
                        "live output reached the client before its scrollback replay"
                    );
                    replayed.extend_from_slice(&bytes);
                }
                ServerMessage::PtyOutput { bytes, .. } => live.extend_from_slice(&bytes),
                _ => {}
            }
        }

        assert_eq!(
            replayed, b"old-history",
            "the replay must carry the history as it was at attach time"
        );
        assert_eq!(
            live, b"live-output",
            "the live chunk must be delivered exactly once, after the replay"
        );
    }

    /// R2: the gate orders, it does not buffer without limit. A pane must never
    /// grow a second history for a client that is not draining its queue, so an
    /// over-full gate reports the client for eviction exactly as a full client
    /// queue does.
    #[test]
    fn a_replay_gate_that_fills_up_reports_the_client_for_eviction() {
        let (client, _receiver, _peer) = queued_test_client(CLIENT_QUEUE_CAPACITY);
        let gate = ReplayGate::pending();
        let chunk = vec![b'x'; 1024 * 1024];

        let mut held = 0;
        while gate.deliver(
            &client,
            ServerMessage::PtyOutput {
                pane: SessionId(1),
                bytes: chunk.clone(),
            },
        ) {
            held += 1;
            assert!(
                held <= REPLAY_HOLD_MAX_BYTES / chunk.len(),
                "the gate held more than its byte cap"
            );
        }

        assert_eq!(held, REPLAY_HOLD_MAX_BYTES / chunk.len());
    }

    /// R8: `try_clone` is `dup(2)` and fails on `EMFILE`. Both of the ones in
    /// `handle_client` sit *below* the registration and *above* the release, so
    /// a transient fd-exhaustion burst used to leak a connection slot per
    /// attempt — permanently, until the cap refused every new connection and the
    /// only cure was restarting the daemon, which destroys every live pane.
    #[test]
    fn a_connection_that_fails_after_registering_releases_its_slot() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let (_client_stream, server_stream) = UnixStream::pair().expect("create socket pair");

        let error = handle_client_with_cloner(server_stream, Arc::clone(&server), |_| {
            Err(io::Error::from_raw_os_error(libc::EMFILE))
        })
        .expect_err("a connection whose fd clone fails must fail");

        assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
        assert!(
            server.lock().expect("server lock").live_clients.is_empty(),
            "a connection that failed after registering must release its slot"
        );
    }

    /// R9: a foreground-process poll must not hold the pane lock across the part
    /// of it that has no time bound.
    ///
    /// The unbounded part in production is the `/proc/<pid>/cmdline` read, which
    /// blocks indefinitely for a process wedged in uninterruptible sleep on a
    /// hung NFS or FUSE mount, and which cannot be injected here. The master
    /// lock, taken immediately before it and released by the same helper, is the
    /// stand-in: holding it stalls the poll at the same point. With the poll
    /// under the pane lock — as it used to be — the pane is then unreachable,
    /// and since `remove_client` and `session_infos` take pane locks *while
    /// holding the server lock*, so is every other pane on the daemon.
    #[test]
    fn a_stalled_foreground_poll_does_not_hold_the_pane_lock() {
        if pty_backed_tests_are_opted_out() {
            return;
        }

        let state = test_pane_state();
        let master = Arc::clone(&state.master);
        let pane: SharedPane = Arc::new(Mutex::new(state));
        let stall = master.lock().expect("hold the master lock");

        let (finished, polled) = mpsc::channel();
        let polling = Arc::clone(&pane);
        let poller = thread::spawn(move || {
            broadcast_foreground_process_if_changed(&polling);
            let _ = finished.send(());
        });
        // Give the poller time to reach the stalled master lock. It is a floor,
        // not a synchronisation point: the assertion below is what decides the
        // test, and it is retried for far longer than this.
        thread::sleep(Duration::from_millis(50));

        drop(lock_pane_within(&pane, DISPATCH_TIMEOUT));

        drop(stall);
        polled
            .recv_timeout(DISPATCH_TIMEOUT)
            .expect("the poll must finish once the master lock is free");
        poller.join().expect("poller thread should not panic");
    }

    /// Replay deadline, review follow-up: the replay deadline covers the whole replay, not each chunk. A
    /// full 5 MiB history is 80 chunks, so a per-chunk deadline let one attach
    /// hold the connection's reader thread for ~400 s while that client's input,
    /// resizes and pings went unread.
    #[test]
    fn a_replay_deadline_covers_the_whole_replay_not_each_chunk() {
        let mut history = RawHistory::new();
        history.append(&history_bytes(3 * RAW_HISTORY_CHUNK_BYTES));
        // One slot, already full, and nothing draining it: every chunk waits out
        // the deadline.
        let (client, _receiver, _peer) = queued_test_client(1);
        client
            .sender
            .try_send(ServerMessage::Sessions(Vec::new()))
            .expect("prime the client queue");

        let budget = Duration::from_millis(200);
        let started = Instant::now();
        send_pty_scrollback_until(&client, SessionId(3), &history.snapshot(), started + budget);
        let elapsed = started.elapsed();

        assert!(
            elapsed < 3 * budget,
            "a three-chunk replay took {elapsed:?}, which is per-chunk rather than per-replay"
        );
    }

    /// Bounded input, review follow-up: the PTY input queue is bounded in bytes as well as in messages.
    /// One `Input` may carry a whole 16 MiB protocol frame, so 64 of them is
    /// ~1 GiB of resident input per pane that a same-uid client can park in any
    /// pane whose child has stopped reading its stdin.
    #[test]
    fn pty_input_is_bounded_in_bytes_not_only_in_messages() {
        // A writer whose `write` blocks until the test releases it: the pane's
        // child that never reads its stdin, without a real PTY.
        struct BlockingWriter {
            release: mpsc::Receiver<()>,
        }

        impl Write for BlockingWriter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                // Returns as soon as the test drops its sender, so the thread is
                // not left parked when the test ends.
                let _ = self.release.recv();
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let (release, blocked) = mpsc::channel();
        let queue = spawn_pty_writer(Box::new(BlockingWriter { release: blocked }));
        let chunk = vec![b'x'; 1024 * 1024];

        let mut accepted = 0;
        let rejection = loop {
            match write_pty_input(&queue, &chunk) {
                Ok(()) => {
                    accepted += 1;
                    assert!(
                        accepted <= PTY_INPUT_QUEUE_CAPACITY,
                        "the byte cap never bit: {accepted} MiB queued"
                    );
                }
                Err(rejection) => break rejection,
            }
        };

        assert_eq!(rejection.code, RejectCode::InputRefused);
        assert_eq!(
            accepted,
            PTY_INPUT_QUEUE_MAX_BYTES / chunk.len(),
            "the queue must fill by bytes, not by message count"
        );
        // Unblock the writer thread so it drains and exits with the test.
        drop(release);
    }

    /// Take `pane`'s lock within `timeout`, failing the test rather than hanging
    /// it if something else is holding the lock it should not be holding.
    fn lock_pane_within(
        pane: &SharedPane,
        timeout: Duration,
    ) -> std::sync::MutexGuard<'_, PaneState> {
        let deadline = Instant::now() + timeout;
        loop {
            match pane.try_lock() {
                Ok(locked) => return locked,
                Err(std::sync::TryLockError::Poisoned(error)) => {
                    panic!("pane lock poisoned: {error}")
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    assert!(
                        Instant::now() < deadline,
                        "the pane lock was still held after {timeout:?}"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn history_bytes(size: usize) -> Vec<u8> {
        (0..size).map(|index| (index % 251) as u8).collect()
    }

    fn concatenated_history(history: &RawHistory) -> Vec<u8> {
        history.snapshot().chunks().flatten().copied().collect()
    }

    fn queued_test_client(
        capacity: usize,
    ) -> (ClientHandle, mpsc::Receiver<ServerMessage>, UnixStream) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let (stream, peer) = UnixStream::pair().expect("create socket pair");
        (
            ClientHandle {
                id: 1,
                sender,
                stream: Arc::new(stream),
            },
            receiver,
            peer,
        )
    }

    /// A pane entry whose replay has already finished: the steady state, and
    /// what every test that is not about attach ordering wants.
    fn open_pane_client(handle: ClientHandle) -> PaneClient {
        PaneClient {
            handle,
            gate: Arc::new(ReplayGate::open()),
        }
    }

    fn test_client(id: ClientId) -> ClientHandle {
        // The receiver and peer socket are dropped immediately; this test only
        // inspects pane.clients membership and never sends to the client.
        let (sender, _) = mpsc::sync_channel(1);
        let (stream, _) = UnixStream::pair().expect("create socket pair");
        ClientHandle {
            id,
            sender,
            stream: Arc::new(stream),
        }
    }

    fn test_pane_state() -> PaneState {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 1,
                cols: 1,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open test pty");
        PaneState {
            session: SessionId(1),
            instance: TEST_INSTANCE,
            name: "test".to_string(),
            title: "test".to_string(),
            rows: 1,
            cols: 1,
            raw_history: RawHistory::new(),
            master: Arc::new(Mutex::new(pair.master)),
            writer: spawn_pty_writer(Box::new(io::sink())),
            child_pid: None,
            foreground_process: ForegroundProcessInfo {
                root_pid: None,
                foreground_pid: None,
                command: None,
            },
            child: None,
            clients: Vec::new(),
            foreground_poll_scheduled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn unique_socket_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-server-test-{unique}.sock"))
    }

    fn unique_socket_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-server-test-{unique}"))
    }
}
