//! `mult-server`, the PTY daemon.
//!
//! # Lock discipline
//!
//! Three mutex families exist and are always acquired in this order:
//!
//! 1. the single [`ServerState`] mutex,
//! 2. one [`PaneState`] mutex per pane,
//! 3. leaves: [`PtyWriteQueue`], the PTY master, and `PaneState::lifecycle_signal`.
//!
//! The rules that keep one wedged pane from freezing the whole daemon:
//!
//! - **L1** Never take the server lock while holding a pane lock. Every handler
//!   acquires server first, then the one pane it routes to.
//! - **L2** Never hold two pane locks at once. `ServerState::pane_by_id` is a
//!   map lookup, not a scan, precisely so a routing decision cannot need a
//!   second pane's mutex.
//! - **L3** Never perform blocking I/O under (1) or (2). PTY writes are handed
//!   to the pane's [`PtyWriteQueue`], whose writer thread holds no mutex while
//!   it writes; client writes go through a `SyncSender`, whose `try_send` never
//!   blocks. The only syscalls left under a lock are non-blocking ioctls
//!   (`TIOCSWINSZ`, `tcgetpgrp`) on the master, which is a leaf.
//! - **L4** Attach replay keeps the pane lock — that barrier is the ordering
//!   guarantee documented in `docs/DAEMON.md` — but releases the server lock
//!   first, so an attach serializes only the pane it attaches to. Replay is
//!   bounded by `RAW_HISTORY_MAX_BYTES` and sends refcounted chunks, so it
//!   neither runs long nor duplicates the retained history.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::{
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use mult_protocol::{
    bounded_screen_dimensions, default_socket_path, ensure_private_dir, read_message,
    write_message, AgentSessionMetadata, AgentStatus, AgentStatusError, AgentStatusOutcome,
    AgentStatusQuery, AgentStatusRecord, AttachError, AttachOutcome, AttachmentLease,
    ClientMessage, ClientScopeId, CreateError, CreateOutcome, ExitInfo, ForegroundProcessInfo,
    IdentityMismatch, LaunchSpec, LeaseOperation, LeaseRejectionReason, OutputSequence, PaneId,
    PaneInfo, RequestId, ServerInstanceId, ServerMessage, SessionId, SessionIdentity, SessionInfo,
    StateNamespace, StopError, StopOutcome, AGENT_STATUS_SCHEMA_VERSION,
    MAX_CACHED_REQUEST_RESULTS_PER_SCOPE, MAX_MESSAGE_BYTES, MAX_PENDING_REQUESTS_PER_CLIENT,
    PROTOCOL_VERSION,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag,
};

type ClientId = u64;
type SharedServer = Arc<Mutex<ServerState>>;
type SharedPane = Arc<Mutex<PaneState>>;
type SharedMasterPty = Arc<Mutex<Box<dyn MasterPty + Send>>>;
type ClientSender = mpsc::SyncSender<ClientDelivery>;

/// One queued item for a client's socket-writer thread.
///
/// Payload-carrying variants keep the pane's refcounted history chunk rather
/// than a private `Vec` copy: an attach used to make the whole retained history
/// resident a second time (once in the pane, once spread over the queued
/// `ReplayChunk` messages). The wire message is built in the writer thread,
/// immediately before it is serialized, so at most one copy exists at a time.
#[derive(Debug)]
enum ClientDelivery {
    Message(ServerMessage),
    Output {
        pane: PaneId,
        lease: AttachmentLease,
        sequence: OutputSequence,
        bytes: Arc<[u8]>,
    },
    Replay {
        request_id: RequestId,
        pane: PaneId,
        lease: AttachmentLease,
        sequence: OutputSequence,
        bytes: Arc<[u8]>,
    },
    Close,
}

const CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// Lines of scrollback the `mult` client's terminal parser retains
/// (`pty.rs::TERMINAL_SCROLLBACK_LINES`). Replay exists to refill that buffer,
/// so it is what bounds retained history.
const RAW_HISTORY_SCROLLBACK_LINES: usize = 5_000;
/// Raw bytes budgeted per retained scrollback line. A line is at most `cols`
/// printable cells plus its wrap/newline and a handful of SGR sequences; 512
/// covers a 200-column line whose every run changes colour, and the cap is a
/// ceiling on a rolling buffer rather than a per-line allocation.
const RAW_HISTORY_BYTES_PER_LINE: usize = 512;
/// Raw PTY output retained per pane for attach replay (~2.4 MiB).
///
/// This used to be `MAX_MESSAGE_BYTES * 2` (32 MiB), which is a wire-frame
/// limit and has nothing to do with what a client can display. Sizing it from
/// the client's actual scrollback need cuts resident daemon memory ~13x per
/// pane while still refilling the deepest scrollback the client keeps.
const RAW_HISTORY_MAX_BYTES: usize = RAW_HISTORY_SCROLLBACK_LINES * RAW_HISTORY_BYTES_PER_LINE;
/// Largest retained-history chunk, and therefore the largest replay message.
const RAW_HISTORY_CHUNK_BYTES: usize = 64 * 1024;
// A replay chunk becomes one wire frame, and the retained history must be worth
// less than a frame's worth of allocation to a client that asks for all of it.
const _: () = assert!(RAW_HISTORY_CHUNK_BYTES < MAX_MESSAGE_BYTES);
const _: () = assert!(RAW_HISTORY_MAX_BYTES < MAX_MESSAGE_BYTES);
/// Bytes of client-supplied input a pane's writer thread may have outstanding.
///
/// A child that stops reading its stdin cannot be made to read it, so the queue
/// has to refuse rather than grow or block. One screenful of paste is a few
/// tens of kilobytes, so a megabyte only fills when the child is genuinely
/// wedged.
const PTY_WRITE_QUEUE_MAX_BYTES: usize = 1024 * 1024;
const CLIENT_QUEUE_CAPACITY: usize = 1_024;
const STOP_TERM_GRACE: Duration = Duration::from_millis(750);
const STOP_FINALIZE_TIMEOUT: Duration = Duration::from_secs(3);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const WAIT_RETRY_DELAY: Duration = Duration::from_millis(50);
/// Longest the daemon waits for every pane to finalize before exiting anyway.
///
/// It must exceed `STOP_TERM_GRACE + STOP_FINALIZE_TIMEOUT` so an ordinary
/// SIGTERM/SIGKILL cycle always completes first, but it must exist: a pane
/// whose stop driver reports `TimedOut` never reaches `Removed`, and an
/// unbounded wait left the daemon alive forever holding its bound socket.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_ERROR_MESSAGE: &str = "mult-server is shutting down";

#[derive(Clone)]
struct ClientHandle {
    id: ClientId,
    sender: ClientSender,
    stream: Arc<UnixStream>,
    active: Arc<AtomicBool>,
}

impl ClientHandle {
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn try_deliver(&self, message: ServerMessage) -> bool {
        self.try_enqueue(ClientDelivery::Message(message))
    }

    /// Queues one delivery without ever blocking.
    ///
    /// Lock rule L3 depends on this: handlers call it while holding the server
    /// and/or pane mutex, so it must be a `try_send` and nothing else.
    fn try_enqueue(&self, delivery: ClientDelivery) -> bool {
        if !self.is_active() {
            return false;
        }
        match self.sender.try_send(delivery) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) | Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    }

    fn finish_after_pending_deliveries(&self) {
        self.active.store(false, Ordering::Release);
        if self.sender.try_send(ClientDelivery::Close).is_err() {
            let _ = self.stream.shutdown(Shutdown::Both);
        }
    }

    fn disconnect(&self) {
        self.active.store(false, Ordering::Release);
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// Builds the wire message for one queued delivery, or `None` for `Close`.
///
/// Called from the client's writer thread so a refcounted payload is copied at
/// most once, at the moment it is serialized.
fn delivery_message(delivery: ClientDelivery) -> Option<ServerMessage> {
    match delivery {
        ClientDelivery::Message(message) => Some(message),
        ClientDelivery::Output {
            pane,
            lease,
            sequence,
            bytes,
        } => Some(ServerMessage::PtyOutput {
            pane,
            lease,
            sequence,
            bytes: bytes.to_vec(),
        }),
        ClientDelivery::Replay {
            request_id,
            pane,
            lease,
            sequence,
            bytes,
        } => Some(ServerMessage::ReplayChunk {
            request_id,
            pane,
            lease,
            sequence,
            bytes: bytes.to_vec(),
        }),
        ClientDelivery::Close => None,
    }
}

/// Why a PTY write was refused instead of queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtyWriteRefusal {
    /// The child is not draining its stdin fast enough (or at all).
    QueueFull,
    /// The pane finalized, or the master rejected a write.
    Closed,
}

/// A pane's bounded, non-blocking inbox for client-supplied PTY input.
///
/// `write_all` + `flush` on a PTY master blocks as soon as the child stops
/// reading, and it used to be called straight from the socket-reader thread
/// while both the server and the pane mutex were held — so one wedged child
/// froze every pane and every client. All writes now go through here: producers
/// only ever push under this leaf mutex (never blocking, never while doing I/O),
/// and one writer thread per pane performs the blocking write holding no mutex
/// at all.
///
/// The queue is bounded and **refuses** rather than drops: silently discarding
/// keystrokes is indistinguishable from a hung terminal. A refusal is reported
/// to the client as `LeaseRejected`, which is conclusive — the client clears the
/// attachment and re-attaches instead of assuming the bytes landed.
struct PtyWriteQueue {
    state: Mutex<PtyWriteQueueState>,
    ready: Condvar,
    capacity: usize,
}

struct PtyWriteQueueState {
    pending: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    closed: bool,
}

impl PtyWriteQueue {
    /// A queue with no writer thread. Production always uses [`Self::spawn`];
    /// this exists so tests can observe exactly what was accepted.
    fn with_capacity(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PtyWriteQueueState {
                pending: VecDeque::new(),
                queued_bytes: 0,
                closed: false,
            }),
            ready: Condvar::new(),
            capacity,
        })
    }

    fn spawn(writer: Box<dyn Write + Send>) -> Arc<Self> {
        let queue = Self::with_capacity(PTY_WRITE_QUEUE_MAX_BYTES);
        let writer_queue = Arc::clone(&queue);
        thread::spawn(move || run_pty_writer(&writer_queue, writer));
        queue
    }

    /// Accepts `bytes` for the PTY, or refuses them. Never blocks.
    fn enqueue(&self, bytes: Vec<u8>) -> Result<(), PtyWriteRefusal> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|_| PtyWriteRefusal::Closed)?;
        if state.closed {
            return Err(PtyWriteRefusal::Closed);
        }
        // Reject the whole write rather than a prefix of it: a partially
        // accepted keystroke sequence is worse than a reported refusal.
        if state.queued_bytes.saturating_add(bytes.len()) > self.capacity {
            return Err(PtyWriteRefusal::QueueFull);
        }
        state.queued_bytes += bytes.len();
        state.pending.push_back(bytes);
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    /// Blocks until there is something to write, or the queue closes.
    ///
    /// Returns `None` once closed: a finalized pane has no child left to
    /// receive the remainder.
    ///
    /// The whole backlog is taken at once and the accounting is reset with it,
    /// so the bound is "queued plus one in-flight batch", not a hard `capacity`
    /// ceiling. That is deliberate: charging the in-flight batch would mean a
    /// wedged child's queue could never refill even after the write completes.
    fn wait_for_writes(&self) -> Option<Vec<Vec<u8>>> {
        let mut state = self.state.lock().ok()?;
        loop {
            if state.closed {
                return None;
            }
            if !state.pending.is_empty() {
                state.queued_bytes = 0;
                return Some(std::mem::take(&mut state.pending).into());
            }
            state = self.ready.wait(state).ok()?;
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.pending.clear();
            state.queued_bytes = 0;
        }
        self.ready.notify_all();
    }

    #[cfg(test)]
    fn queued_bytes(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.queued_bytes)
            .unwrap_or(0)
    }
}

/// Drains one pane's write queue. Holds no mutex across the blocking write.
fn run_pty_writer(queue: &Arc<PtyWriteQueue>, mut writer: Box<dyn Write + Send>) {
    while let Some(batch) = queue.wait_for_writes() {
        for bytes in batch {
            if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                eprintln!("failed to write PTY input: {error}");
                // Further writes cannot succeed either. Closing makes the next
                // client write a reported LeaseRejected instead of a silent
                // discard.
                queue.close();
                return;
            }
        }
    }
}

/// Raw PTY output retained per pane for attach replay.
///
/// A flat `Vec<u8>` made trimming O(retained history): once the cap was
/// reached, every 8 KiB read `drain(..overflow)`d — a memmove of the whole
/// buffer — under the pane lock. This keeps the history as a deque of immutable
/// refcounted chunks plus an offset into the oldest one, so a trim drops whole
/// chunks in O(bytes dropped) and never touches a retained byte. The chunks are
/// also exactly what replay sends, so a queued replay shares them with the pane
/// instead of making the history resident twice.
#[derive(Default)]
struct RawHistory {
    chunks: VecDeque<Arc<[u8]>>,
    /// Bytes of `chunks.front()` already trimmed away. Always strictly less
    /// than that chunk's length while any chunk remains.
    front_offset: usize,
    len: usize,
}

impl RawHistory {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }

    /// Appends `bytes` (splitting it into replay-sized chunks) and trims the
    /// oldest bytes beyond `limit`. Returns the number of bytes dropped.
    fn append(&mut self, bytes: &Arc<[u8]>, limit: usize) -> usize {
        if bytes.len() <= RAW_HISTORY_CHUNK_BYTES {
            // The reader's 8 KiB buffer always lands here, so a live read costs
            // one allocation and no copy beyond the one that made the `Arc`.
            if !bytes.is_empty() {
                self.len += bytes.len();
                self.chunks.push_back(Arc::clone(bytes));
            }
        } else {
            for piece in bytes.chunks(RAW_HISTORY_CHUNK_BYTES) {
                self.len += piece.len();
                self.chunks.push_back(Arc::from(piece));
            }
        }
        self.trim_to(limit)
    }

    fn trim_to(&mut self, limit: usize) -> usize {
        let mut remaining = self.len.saturating_sub(limit);
        let mut dropped = 0;
        while remaining > 0 {
            let Some(front) = self.chunks.front() else {
                break;
            };
            let available = front.len() - self.front_offset;
            if remaining < available {
                self.front_offset += remaining;
                self.len -= remaining;
                dropped += remaining;
                break;
            }
            self.chunks.pop_front();
            self.front_offset = 0;
            self.len -= available;
            dropped += available;
            remaining -= available;
        }
        dropped
    }

    /// The retained history as replay-sized chunks, oldest first.
    ///
    /// Every chunk but the oldest is shared with the pane at zero cost. Only
    /// the oldest needs a copy, and only when a trim landed inside it.
    fn replay_chunks(&self) -> Vec<Arc<[u8]>> {
        let mut chunks = Vec::with_capacity(self.chunks.len());
        for (index, chunk) in self.chunks.iter().enumerate() {
            if index == 0 && self.front_offset > 0 {
                chunks.push(Arc::from(&chunk[self.front_offset..]));
            } else {
                chunks.push(Arc::clone(chunk));
            }
        }
        chunks
    }

    #[cfg(test)]
    fn to_vec(&self) -> Vec<u8> {
        self.replay_chunks().concat()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RequestKey {
    scope: ClientScopeId,
    request_id: RequestId,
}

struct RequestRecord {
    request: ClientMessage,
    response: Option<ServerMessage>,
    waiters: Vec<ClientHandle>,
}

struct RequestScopeState {
    highest_request_id: Option<RequestId>,
    records: BTreeMap<RequestId, RequestRecord>,
}

enum RequestDisposition {
    New,
    Pending,
    Cached(ServerMessage),
    Collision,
    Expired,
    TooManyPending,
}

struct ServerState {
    sessions: BTreeMap<SessionId, SharedPane>,
    session_by_identity: BTreeMap<SessionIdentity, SessionId>,
    reserved_sessions: BTreeSet<SessionId>,
    reserved_identities: BTreeSet<SessionIdentity>,
    agent_states: BTreeMap<SessionIdentity, DaemonAgentState>,
    next_session_id: u64,
    next_client_id: ClientId,
    next_lease: Option<AttachmentLease>,
    server_instance: ServerInstanceId,
    request_scopes: BTreeMap<ClientScopeId, RequestScopeState>,
    shutting_down: bool,
}

#[derive(Clone)]
struct AttachmentOwner {
    scope: ClientScopeId,
    lease: AttachmentLease,
    client: ClientHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneLifecycle {
    Running,
    Stopping,
    Exited,
    Removed,
}

/// One pane's mutable state.
///
/// Lock rules (see the module header): this mutex is taken **after** the server
/// mutex and never with another pane's mutex held. Nothing under it blocks —
/// PTY writes go to `writes`, client writes go through `ClientHandle::try_*`.
/// The one long-running section is attach replay, which is bounded by
/// `RAW_HISTORY_MAX_BYTES` and deliberately holds this mutex so replay cannot
/// interleave with live output (`docs/DAEMON.md`, "Attach replay ordering").
struct PaneState {
    session: SessionId,
    identity: SessionIdentity,
    agent: Option<AgentSessionMetadata>,
    /// Always `PaneId(session.0)`. The daemon allocates one pane per session
    /// and the two numeric coordinates are the same number by construction;
    /// `ServerState::pane_by_id` relies on it.
    pane: PaneId,
    name: String,
    title: String,
    rows: u16,
    cols: u16,
    raw_history: RawHistory,
    history_start: OutputSequence,
    next_output: OutputSequence,
    master: SharedMasterPty,
    writes: Arc<PtyWriteQueue>,
    child_pid: u32,
    process_group: libc::pid_t,
    foreground_process: ForegroundProcessInfo,
    owner: Option<AttachmentOwner>,
    lifecycle: PaneLifecycle,
    reader_done: bool,
    child_exit: Option<ExitInfo>,
    last_wait_error: Option<String>,
    stop_driver_active: bool,
    pending_stops: Vec<RequestKey>,
    lifecycle_signal: Arc<(Mutex<u64>, Condvar)>,
    foreground_poll_scheduled: Arc<AtomicBool>,
}

struct SessionCreateSpec {
    requested_id: Option<SessionId>,
    pane: PaneSpawnSpec,
}

#[derive(Debug, Clone)]
struct DaemonAgentState {
    metadata: AgentSessionMetadata,
    status: Option<AgentStatusRecord>,
}

struct PaneSpawnSpec {
    identity: SessionIdentity,
    agent: Option<AgentSessionMetadata>,
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
    child: Box<dyn Child + Send + Sync>,
}

fn main() -> io::Result<()> {
    ignore_hangup_signal()?;
    let shutdown = install_shutdown_signals()?;
    let socket_path = default_socket_path();
    bind_socket_path(&socket_path)?;
    let server = Arc::new(Mutex::new(ServerState::new()?));
    let listener = bind_unix_listener(&socket_path)?;
    // From here on every exit path — `?`, a shutdown deadline, or a panic —
    // unlinks the socket. Leaving it bound made the next client autospawn
    // connect to nothing.
    let _socket = SocketGuard::new(socket_path.clone());
    listener.set_nonblocking(true)?;
    restrict_socket_permissions(&socket_path)?;
    eprintln!("mult-server listening on {}", socket_path.display());

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let server = Arc::clone(&server);
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, server) {
                        eprintln!("client error: {error}");
                    }
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => eprintln!("accept error: {error}"),
        }
    }

    begin_daemon_shutdown(&server);
    if !wait_for_sessions_drained(&server, SHUTDOWN_DRAIN_TIMEOUT) {
        eprintln!(
            "mult-server shutdown deadline reached with sessions still live; exiting and unlinking {}",
            socket_path.display()
        );
    }
    Ok(())
}

/// Removes the listening socket when the daemon leaves `main`, however it
/// leaves.
struct SocketGuard {
    path: PathBuf,
}

impl SocketGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Waits for every session and reservation to finalize, bounded by `timeout`.
///
/// Returns whether the daemon drained. The deadline is the point: a pane whose
/// stop driver reports `TerminationResult::TimedOut` is never removed, and the
/// old unbounded `sessions.is_empty()` spin meant such a pane kept `mult-server`
/// alive — and its socket bound — forever.
fn wait_for_sessions_drained(server: &SharedServer, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let drained = match server.lock() {
            Ok(state) => state.sessions.is_empty() && state.reserved_sessions.is_empty(),
            // A poisoned lock will never report an empty map. Exiting is the
            // only remaining way to release the socket.
            Err(_) => return false,
        };
        if drained {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        thread::sleep(ACCEPT_POLL_INTERVAL.min(remaining));
    }
}

impl ServerState {
    fn new() -> io::Result<Self> {
        Ok(Self {
            sessions: BTreeMap::new(),
            session_by_identity: BTreeMap::new(),
            reserved_sessions: BTreeSet::new(),
            reserved_identities: BTreeSet::new(),
            agent_states: BTreeMap::new(),
            next_session_id: 1,
            next_client_id: 1,
            next_lease: Some(AttachmentLease::MIN),
            server_instance: ServerInstanceId::from_bytes(random_bytes()?),
            request_scopes: BTreeMap::new(),
            shutting_down: false,
        })
    }

    fn allocate_client_id(&mut self) -> io::Result<ClientId> {
        let id = self.next_client_id;
        self.next_client_id = self
            .next_client_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("client ID space exhausted"))?;
        Ok(id)
    }

    fn allocate_scope(&mut self) -> io::Result<ClientScopeId> {
        for _ in 0..16 {
            let scope = ClientScopeId::from_bytes(random_bytes()?);
            if let std::collections::btree_map::Entry::Vacant(entry) =
                self.request_scopes.entry(scope)
            {
                entry.insert(RequestScopeState::new());
                return Ok(scope);
            }
        }
        Err(io::Error::other("failed to allocate a unique client scope"))
    }

    fn resume_or_allocate_scope(
        &mut self,
        resume: Option<ClientScopeId>,
    ) -> io::Result<(ClientScopeId, bool)> {
        if let Some(scope) = resume.filter(|scope| self.request_scopes.contains_key(scope)) {
            return Ok((scope, true));
        }
        self.allocate_scope().map(|scope| (scope, false))
    }

    fn allocate_lease(&mut self) -> io::Result<AttachmentLease> {
        let lease = self
            .next_lease
            .ok_or_else(|| io::Error::other("attachment lease space exhausted"))?;
        self.next_lease = lease.checked_next();
        Ok(lease)
    }

    fn allocate_session_id(&mut self) -> io::Result<SessionId> {
        loop {
            let session = SessionId(self.next_session_id);
            self.next_session_id = self
                .next_session_id
                .checked_add(1)
                .ok_or_else(|| io::Error::other("session ID space exhausted"))?;
            if !self.sessions.contains_key(&session) && !self.reserved_sessions.contains(&session) {
                return Ok(session);
            }
        }
    }

    fn reserve_session(
        &mut self,
        requested_id: Option<SessionId>,
        identity: SessionIdentity,
    ) -> io::Result<SessionId> {
        if self.shutting_down {
            return Err(io::Error::other(SHUTDOWN_ERROR_MESSAGE));
        }
        if self.session_by_identity.contains_key(&identity)
            || self.reserved_identities.contains(&identity)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "logical session identity already exists or is being created",
            ));
        }
        let session = match requested_id {
            Some(session) => session,
            None => self.allocate_session_id()?,
        };
        if self.sessions.contains_key(&session) || !self.reserved_sessions.insert(session) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("session {} already exists or is being created", session.0),
            ));
        }
        self.reserved_identities.insert(identity);
        Ok(session)
    }

    fn release_session_reservation(&mut self, session: SessionId, identity: SessionIdentity) {
        self.reserved_sessions.remove(&session);
        self.reserved_identities.remove(&identity);
    }

    fn publish_reserved_session(&mut self, session: SessionId, pane: SharedPane) -> io::Result<()> {
        if self.shutting_down {
            return Err(io::Error::other(SHUTDOWN_ERROR_MESSAGE));
        }
        let (identity, agent) = {
            let pane = pane.lock().map_err(lock_error)?;
            (pane.identity, pane.agent)
        };
        if self.sessions.contains_key(&session)
            || self.session_by_identity.contains_key(&identity)
            || !self.reserved_sessions.contains(&session)
            || !self.reserved_identities.contains(&identity)
        {
            return Err(io::Error::other("session was created concurrently"));
        }
        self.release_session_reservation(session, identity);
        self.session_by_identity.insert(identity, session);
        match agent {
            Some(metadata) => {
                self.agent_states.insert(
                    identity,
                    DaemonAgentState {
                        metadata,
                        status: None,
                    },
                );
            }
            None => {
                self.agent_states.remove(&identity);
            }
        }
        self.sessions.insert(session, pane);
        Ok(())
    }

    fn session_infos(&self, namespace: StateNamespace) -> Vec<SessionInfo> {
        self.sessions
            .values()
            .filter_map(|pane| {
                pane.lock().ok().and_then(|pane| {
                    (pane.identity.namespace == namespace).then(|| pane.session_info())
                })
            })
            .collect()
    }

    /// Routes a `PaneId` to its pane.
    ///
    /// `PaneId` and `SessionId` are two names for the same daemon coordinate:
    /// `spawn_pane` sets `pane = PaneId(session.0)` and nothing ever changes it,
    /// so the map lookup is exhaustive. (They stay distinct wire newtypes
    /// because `mult_protocol` and the client both use them; collapsing them is
    /// a protocol change, not a daemon change.)
    ///
    /// This used to fall back to a linear scan that locked **every** pane while
    /// holding the server lock, on every `Input`, `Resize` and `Detach`. The
    /// fallback could never match anything the lookup had missed, and it
    /// violated lock rule L2 — one pane blocked in the scan blocked routing for
    /// all of them.
    fn pane_by_id(&self, pane: PaneId) -> Option<SharedPane> {
        self.sessions.get(&SessionId(pane.0)).cloned()
    }

    fn remove_session_if_same(&mut self, session: SessionId, pane: &SharedPane) -> bool {
        if self
            .sessions
            .get(&session)
            .is_some_and(|existing| Arc::ptr_eq(existing, pane))
        {
            self.sessions.remove(&session);
            self.session_by_identity.retain(|_, id| *id != session);
            true
        } else {
            false
        }
    }

    fn session_for_identity(&self, identity: SessionIdentity) -> Option<SharedPane> {
        let session = self.session_by_identity.get(&identity)?;
        self.sessions.get(session).cloned()
    }

    fn record_agent_exit(
        &mut self,
        identity: SessionIdentity,
        metadata: AgentSessionMetadata,
        exit: &ExitInfo,
    ) {
        let Some(state) = self.agent_states.get_mut(&identity) else {
            return;
        };
        if state.metadata != metadata || state.status.is_some_and(|status| status.status.is_final())
        {
            return;
        }
        state.status = Some(AgentStatusRecord {
            schema_version: metadata.schema_version,
            identity,
            chat_id: metadata.chat_id,
            agent: metadata.agent,
            generation: metadata.generation,
            status: if exit.code == 0 {
                AgentStatus::Exited
            } else {
                AgentStatus::Failed
            },
        });
    }

    fn begin_request(
        &mut self,
        scope: ClientScopeId,
        request: &ClientMessage,
        client: &ClientHandle,
    ) -> RequestDisposition {
        let request_id = request_id(request).expect("stateful request has an ID");
        let scope_state = self
            .request_scopes
            .entry(scope)
            .or_insert_with(RequestScopeState::new);
        scope_state.begin(request_id, request, client)
    }

    fn complete_request(&mut self, key: RequestKey, response: ServerMessage) -> Vec<ClientHandle> {
        self.request_scopes
            .get_mut(&key.scope)
            .map(|scope| scope.complete(key.request_id, response))
            .unwrap_or_default()
    }

    fn remove_client(&mut self, client_id: ClientId) {
        for scope in self.request_scopes.values_mut() {
            for record in scope.records.values_mut() {
                record.waiters.retain(|client| client.id != client_id);
            }
        }
        // Keep logical attachment ownership across transport loss. A resumed
        // exact attach retry can transfer the lease to the replacement
        // connection; explicit Detach, takeover, or finalization invalidates it.
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new().expect("server identity randomness must be available")
    }
}

impl RequestScopeState {
    fn new() -> Self {
        Self {
            highest_request_id: None,
            records: BTreeMap::new(),
        }
    }

    fn begin(
        &mut self,
        request_id: RequestId,
        request: &ClientMessage,
        client: &ClientHandle,
    ) -> RequestDisposition {
        if let Some(record) = self.records.get_mut(&request_id) {
            if record.request != *request {
                return RequestDisposition::Collision;
            }
            if let Some(response) = &record.response {
                return RequestDisposition::Cached(response.clone());
            }
            if !record.waiters.iter().any(|waiter| waiter.id == client.id) {
                record.waiters.push(client.clone());
            }
            return RequestDisposition::Pending;
        }

        if self
            .highest_request_id
            .is_some_and(|highest| request_id <= highest)
        {
            return RequestDisposition::Expired;
        }
        let pending = self
            .records
            .values()
            .filter(|record| record.response.is_none())
            .count();
        let overloaded = pending >= MAX_PENDING_REQUESTS_PER_CLIENT;
        // Even an overload rejection consumes the request ID. Otherwise the
        // same ID could be rejected now and mutate state later after capacity
        // becomes available, violating scoped idempotence.
        self.highest_request_id = Some(request_id);
        self.records.insert(
            request_id,
            RequestRecord {
                request: request.clone(),
                response: None,
                waiters: vec![client.clone()],
            },
        );
        if overloaded {
            RequestDisposition::TooManyPending
        } else {
            RequestDisposition::New
        }
    }

    fn complete(&mut self, request_id: RequestId, response: ServerMessage) -> Vec<ClientHandle> {
        let waiters = if let Some(record) = self.records.get_mut(&request_id) {
            record.response = Some(response);
            std::mem::take(&mut record.waiters)
        } else {
            Vec::new()
        };
        self.evict_old_results();
        waiters
    }

    fn evict_old_results(&mut self) {
        while self
            .records
            .values()
            .filter(|record| record.response.is_some())
            .count()
            > MAX_CACHED_REQUEST_RESULTS_PER_SCOPE
        {
            let Some(oldest) = self
                .records
                .iter()
                .find_map(|(id, record)| record.response.is_some().then_some(*id))
            else {
                break;
            };
            self.records.remove(&oldest);
        }
    }
}

fn request_id(message: &ClientMessage) -> Option<RequestId> {
    match message {
        ClientMessage::CreateSession { request_id, .. }
        | ClientMessage::Attach { request_id, .. }
        | ClientMessage::Stop { request_id, .. }
        | ClientMessage::UpdateAgentStatus { request_id, .. }
        | ClientMessage::GetAgentStatus { request_id, .. } => Some(*request_id),
        _ => None,
    }
}

fn random_bytes() -> io::Result<[u8; 16]> {
    let mut bytes = [0; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn ignore_hangup_signal() -> io::Result<()> {
    if unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn install_shutdown_signals() -> io::Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        flag::register_conditional_shutdown(signal, 128 + signal, Arc::clone(&shutdown))?;
        flag::register(signal, Arc::clone(&shutdown))?;
    }
    Ok(shutdown)
}

fn begin_daemon_shutdown(server: &SharedServer) {
    let panes = match server.lock() {
        Ok(mut state) => {
            state.shutting_down = true;
            state.sessions.values().cloned().collect::<Vec<_>>()
        }
        Err(_) => return,
    };
    for pane in panes {
        start_stop_if_needed(Arc::clone(server), pane);
    }
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
        unsafe { libc::umask(self.previous) };
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

fn verify_peer_owner(stream: &UnixStream, peer_label: &str) -> io::Result<()> {
    let Some(peer_uid) = peer_uid(stream)? else {
        return Ok(());
    };
    let current_uid = current_euid();
    if peer_uid == current_uid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("rejecting {peer_label} uid {peer_uid}; expected current uid {current_uid}"),
        ))
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<Option<u32>> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    if length < std::mem::size_of::<libc::ucred>() as libc::socklen_t {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short SO_PEERCRED response",
        ));
    }
    Ok(Some(unsafe { credentials.assume_init().uid }))
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(_stream: &UnixStream) -> io::Result<Option<u32>> {
    Ok(None)
}

fn current_euid() -> u32 {
    unsafe { libc::geteuid() as u32 }
}

fn spawn_pane(session: SessionId, spec: PaneSpawnSpec) -> io::Result<SpawnedPane> {
    let (rows, cols) = bounded_pty_dimensions(spec.rows, spec.cols);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(error_to_io)?;

    // Complete every fallible master-side setup step before spawning. Once the
    // child exists, either the waiter owns it or the unpublished cleanup path
    // below kills and reaps it exactly once.
    let reader = pair.master.try_clone_reader().map_err(error_to_io)?;
    // Taken here because it is fallible; the writer thread is only started once
    // the child is published, so a failed spawn cannot leak one.
    let writer = pair.master.take_writer().map_err(error_to_io)?;

    let shell = default_shell();
    let mut command = CommandBuilder::new(&shell);
    if let LaunchSpec::Command(command_line) = &spec.launch {
        command.args(shell_command_args(command_line.clone()));
    }
    if let Some(cwd) = &spec.cwd {
        command.cwd(cwd.as_os_str());
    }
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    for (key, value) in spec.env {
        command.env(key, value);
    }

    let mut child = pair.slave.spawn_command(command).map_err(error_to_io)?;
    let child_pid = match child.process_id() {
        Some(pid) if pid > 1 && pid <= libc::pid_t::MAX as u32 => pid,
        _ => {
            cleanup_unpublished_child(&mut child, None);
            return Err(io::Error::other(
                "PTY child did not report a safe process ID",
            ));
        }
    };
    let process_group = match validate_child_process_group(child_pid) {
        Ok(group) => group,
        Err(error) => {
            cleanup_unpublished_child(&mut child, Some(child_pid as libc::pid_t));
            return Err(error);
        }
    };

    let master = Arc::new(Mutex::new(pair.master));
    let title = pane_title(&shell, &spec.launch);
    let pane = Arc::new(Mutex::new(PaneState {
        session,
        identity: spec.identity,
        agent: spec.agent,
        pane: PaneId(session.0),
        name: spec.name,
        title,
        rows,
        cols,
        raw_history: RawHistory::default(),
        history_start: OutputSequence::ZERO,
        next_output: OutputSequence::ZERO,
        master,
        writes: PtyWriteQueue::spawn(writer),
        child_pid,
        process_group,
        foreground_process: ForegroundProcessInfo {
            root_pid: Some(child_pid),
            foreground_pid: None,
            command: None,
        },
        owner: None,
        lifecycle: PaneLifecycle::Running,
        reader_done: false,
        child_exit: None,
        last_wait_error: None,
        stop_driver_active: false,
        pending_stops: Vec::new(),
        lifecycle_signal: Arc::new((Mutex::new(0), Condvar::new())),
        foreground_poll_scheduled: Arc::new(AtomicBool::new(false)),
    }));

    Ok(SpawnedPane {
        pane,
        reader,
        child,
    })
}

fn validate_child_process_group(child_pid: u32) -> io::Result<libc::pid_t> {
    let pid = child_pid as libc::pid_t;
    let group = unsafe { libc::getpgid(pid) };
    if group == -1 {
        return Err(io::Error::last_os_error());
    }
    let session = unsafe { libc::getsid(pid) };
    if session == -1 {
        return Err(io::Error::last_os_error());
    }
    let own_group = unsafe { libc::getpgrp() };
    if group != pid || session != pid || group == own_group || group <= 1 {
        return Err(io::Error::other(format!(
            "PTY child {pid} is not an isolated session/process-group leader"
        )));
    }
    Ok(group)
}

fn cleanup_unpublished_child(
    child: &mut Box<dyn Child + Send + Sync>,
    process_group: Option<libc::pid_t>,
) {
    if let Some(group) = process_group {
        let _ = signal_process_group(group, libc::SIGKILL);
    }
    // Always target the direct child too. The caller may know only a candidate
    // group (for example when group validation itself failed), and ESRCH for
    // that candidate must not leave a live unpublished child blocking wait.
    let _ = child.kill();
    // This local path is the sole owner of an unpublished child. Never drop
    // that handle unreaped merely because wait was interrupted or transiently
    // failed.
    loop {
        match child.wait() {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                eprintln!("failed to reap unpublished PTY child; retrying: {error}");
                thread::sleep(WAIT_RETRY_DELAY);
            }
        }
    }
}

fn handle_client(stream: UnixStream, server: SharedServer) -> io::Result<()> {
    verify_peer_owner(&stream, "client")?;
    let (sender, receiver) = mpsc::sync_channel(CLIENT_QUEUE_CAPACITY);
    let client_id = server.lock().map_err(lock_error)?.allocate_client_id()?;
    let shutdown_handle = Arc::new(stream.try_clone()?);
    let active = Arc::new(AtomicBool::new(true));
    let client = ClientHandle {
        id: client_id,
        sender: sender.clone(),
        stream: Arc::clone(&shutdown_handle),
        active: Arc::clone(&active),
    };

    let mut writer_stream = stream.try_clone()?;
    let writer_server = Arc::clone(&server);
    thread::spawn(move || {
        while let Ok(delivery) = receiver.recv() {
            let Some(message) = delivery_message(delivery) else {
                break;
            };
            if write_message(&mut writer_stream, &message).is_err() {
                break;
            }
        }
        active.store(false, Ordering::Release);
        if let Ok(mut state) = writer_server.lock() {
            state.remove_client(client_id);
        }
        let _ = shutdown_handle.shutdown(Shutdown::Both);
    });

    let result = handle_client_messages(stream, &server, client.clone());
    client.finish_after_pending_deliveries();
    if let Ok(mut state) = server.lock() {
        state.remove_client(client_id);
    }
    drop(sender);
    result
}

fn handle_client_messages(
    mut stream: UnixStream,
    server: &SharedServer,
    client: ClientHandle,
) -> io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_HELLO_TIMEOUT))?;
    let Some(message) = read_client_message(&mut stream)? else {
        return Ok(());
    };
    stream.set_read_timeout(None)?;

    let ClientMessage::Hello {
        protocol_version,
        resume,
    } = message
    else {
        let _ = client.try_deliver(ServerMessage::Error {
            message: "expected protocol hello before other client messages".to_string(),
        });
        return Ok(());
    };
    if protocol_version != PROTOCOL_VERSION {
        let _ = client.try_deliver(ServerMessage::Error {
            message: format!(
                "client protocol version {protocol_version} is incompatible with server version {PROTOCOL_VERSION}; restart mult clients"
            ),
        });
        return Ok(());
    }
    let (scope, resumed, server_instance) = {
        let mut state = server.lock().map_err(lock_error)?;
        let (scope, resumed) = state.resume_or_allocate_scope(resume)?;
        (scope, resumed, state.server_instance)
    };
    if !client.try_deliver(ServerMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        server_instance,
        client_scope: scope,
        resumed,
    }) {
        return Ok(());
    }

    while let Some(message) = read_client_message(&mut stream)? {
        match message {
            ClientMessage::Hello { .. } => {
                let _ = client.try_deliver(ServerMessage::Error {
                    message: "protocol hello may only be sent once per connection".to_string(),
                });
                break;
            }
            ClientMessage::ListSessions { namespace } => {
                let sessions = server.lock().map_err(lock_error)?.session_infos(namespace);
                let _ = client.try_deliver(ServerMessage::Sessions {
                    namespace,
                    sessions,
                });
            }
            request @ ClientMessage::CreateSession { .. } => {
                handle_create_request(server, scope, &client, request)?;
            }
            request @ ClientMessage::Attach { .. } => {
                handle_attach_request(server, scope, resumed, &client, request)?;
            }
            ClientMessage::Input { pane, lease, bytes } => {
                handle_leased_input(
                    server,
                    scope,
                    &client,
                    pane,
                    lease,
                    bytes,
                    LeaseOperation::Input,
                );
            }
            ClientMessage::Paste { pane, lease, bytes } => {
                handle_leased_input(
                    server,
                    scope,
                    &client,
                    pane,
                    lease,
                    bytes,
                    LeaseOperation::Paste,
                );
            }
            ClientMessage::Scroll { .. }
            | ClientMessage::ScrollToTop { .. }
            | ClientMessage::ScrollToBottom { .. } => {}
            ClientMessage::Resize {
                pane,
                lease,
                rows,
                cols,
            } => handle_leased_resize(server, scope, &client, pane, lease, rows, cols),
            ClientMessage::Detach { pane, lease } => {
                handle_leased_detach(server, scope, &client, pane, lease)
            }
            request @ ClientMessage::Stop { .. } => {
                handle_stop_request(server, scope, &client, request)?;
            }
            request @ (ClientMessage::UpdateAgentStatus { .. }
            | ClientMessage::GetAgentStatus { .. }) => {
                handle_agent_status_request(server, scope, &client, request)?;
            }
        }
    }
    Ok(())
}

fn handle_create_request(
    server: &SharedServer,
    scope: ClientScopeId,
    client: &ClientHandle,
    request: ClientMessage,
) -> io::Result<()> {
    let ClientMessage::CreateSession {
        request_id,
        identity,
        requested_id,
        agent,
        name,
        cwd,
        env,
        launch,
        rows,
        cols,
    } = &request
    else {
        unreachable!();
    };
    match server
        .lock()
        .map_err(lock_error)?
        .begin_request(scope, &request, client)
    {
        RequestDisposition::Cached(response) => {
            let _ = client.try_deliver(response);
            return Ok(());
        }
        RequestDisposition::Pending => return Ok(()),
        RequestDisposition::Collision => {
            return deliver_create_error(client, *request_id, CreateError::RequestCollision)
        }
        RequestDisposition::Expired => {
            return deliver_create_error(client, *request_id, CreateError::RetryExpired)
        }
        RequestDisposition::TooManyPending => {
            let response = ServerMessage::CreateResult {
                request_id: *request_id,
                outcome: CreateOutcome::Error(CreateError::Failed {
                    message: "too many pending requests".to_string(),
                }),
            };
            complete_and_deliver(
                server,
                RequestKey {
                    scope,
                    request_id: *request_id,
                },
                response,
            );
            return Ok(());
        }
        RequestDisposition::New => {}
    }

    let invalid_agent = agent.and_then(validate_agent_metadata);
    let existing_numeric = requested_id.and_then(|id| {
        server
            .lock()
            .ok()
            .and_then(|state| state.sessions.get(&id).cloned())
    });
    let existing_identity = server
        .lock()
        .ok()
        .and_then(|state| state.session_for_identity(*identity));
    let outcome = if let Some(error) = invalid_agent {
        CreateOutcome::Error(CreateError::InvalidAgentMetadata(error))
    } else if let Some(pane) = existing_numeric {
        let pane = pane.lock().map_err(lock_error)?;
        if pane.identity == *identity {
            CreateOutcome::Error(CreateError::SessionAlreadyExists {
                session: pane.session_info(),
            })
        } else {
            CreateOutcome::Error(CreateError::IdentityMismatch {
                session: pane.session,
                mismatch: identity_mismatch(pane.identity, *identity),
            })
        }
    } else if let Some(pane) = existing_identity {
        CreateOutcome::Error(CreateError::IdentityAlreadyExists {
            session: pane.lock().map_err(lock_error)?.session_info(),
        })
    } else {
        match create_session(
            server,
            SessionCreateSpec {
                requested_id: *requested_id,
                pane: PaneSpawnSpec {
                    identity: *identity,
                    agent: *agent,
                    name: name.clone(),
                    cwd: cwd.clone(),
                    env: env.clone(),
                    launch: launch.clone(),
                    rows: *rows,
                    cols: *cols,
                },
            },
        ) {
            Ok(pane) => CreateOutcome::Created {
                session: pane.lock().map_err(lock_error)?.session_info(),
            },
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let numeric = requested_id.and_then(|id| {
                    server
                        .lock()
                        .ok()
                        .and_then(|state| state.sessions.get(&id).cloned())
                });
                let logical = server
                    .lock()
                    .ok()
                    .and_then(|state| state.session_for_identity(*identity));
                if let Some(pane) = numeric {
                    let pane = pane.lock().map_err(lock_error)?;
                    if pane.identity == *identity {
                        CreateOutcome::Error(CreateError::SessionAlreadyExists {
                            session: pane.session_info(),
                        })
                    } else {
                        CreateOutcome::Error(CreateError::IdentityMismatch {
                            session: pane.session,
                            mismatch: identity_mismatch(pane.identity, *identity),
                        })
                    }
                } else if let Some(pane) = logical {
                    CreateOutcome::Error(CreateError::IdentityAlreadyExists {
                        session: pane.lock().map_err(lock_error)?.session_info(),
                    })
                } else {
                    CreateOutcome::Error(CreateError::Failed {
                        message: error.to_string(),
                    })
                }
            }
            Err(error) => CreateOutcome::Error(CreateError::Failed {
                message: error.to_string(),
            }),
        }
    };
    let response = ServerMessage::CreateResult {
        request_id: *request_id,
        outcome,
    };
    complete_and_deliver(
        server,
        RequestKey {
            scope,
            request_id: *request_id,
        },
        response,
    );
    Ok(())
}

fn validate_agent_metadata(metadata: AgentSessionMetadata) -> Option<AgentStatusError> {
    if metadata.schema_version != AGENT_STATUS_SCHEMA_VERSION {
        return Some(AgentStatusError::WrongSchema {
            expected: AGENT_STATUS_SCHEMA_VERSION,
            received: metadata.schema_version,
        });
    }
    if metadata.chat_id == 0 {
        return Some(AgentStatusError::WrongChat {
            expected: 1,
            received: 0,
        });
    }
    None
}

fn identity_mismatch(expected: SessionIdentity, received: SessionIdentity) -> IdentityMismatch {
    if expected.namespace != received.namespace {
        IdentityMismatch::Namespace
    } else {
        IdentityMismatch::SessionToken
    }
}

fn deliver_create_error(
    client: &ClientHandle,
    request_id: RequestId,
    error: CreateError,
) -> io::Result<()> {
    let _ = client.try_deliver(ServerMessage::CreateResult {
        request_id,
        outcome: CreateOutcome::Error(error),
    });
    Ok(())
}

fn handle_attach_request(
    server: &SharedServer,
    scope: ClientScopeId,
    resumed: bool,
    client: &ClientHandle,
    request: ClientMessage,
) -> io::Result<()> {
    handle_attach_request_with_hooks(server, scope, resumed, client, request, || {}, || {})
}

/// `before_commit` runs at the attach/shutdown linearization boundary;
/// `before_replay` runs once the server lock has been released but while the
/// pane barrier is still held, which is exactly the state lock rule L4
/// requires. Both are no-ops in production.
fn handle_attach_request_with_hooks(
    server: &SharedServer,
    scope: ClientScopeId,
    resumed: bool,
    client: &ClientHandle,
    request: ClientMessage,
    before_commit: impl FnOnce(),
    before_replay: impl FnOnce(),
) -> io::Result<()> {
    let ClientMessage::Attach {
        request_id,
        identity,
        session,
        rows,
        cols,
    } = &request
    else {
        unreachable!();
    };
    let key = RequestKey {
        scope,
        request_id: *request_id,
    };

    let disposition = server
        .lock()
        .map_err(lock_error)?
        .begin_request(scope, &request, client);
    let cached = match disposition {
        RequestDisposition::Cached(response) => Some(response),
        RequestDisposition::Pending => return Ok(()),
        RequestDisposition::Collision => {
            return deliver_attach_error(client, *request_id, AttachError::RequestCollision)
        }
        RequestDisposition::Expired => {
            return deliver_attach_error(client, *request_id, AttachError::RetryExpired)
        }
        RequestDisposition::TooManyPending => {
            let response = ServerMessage::AttachResult {
                request_id: *request_id,
                outcome: AttachOutcome::Error(AttachError::Failed {
                    message: "too many pending requests".to_string(),
                }),
            };
            complete_and_deliver(server, key, response);
            return Ok(());
        }
        RequestDisposition::New => None,
    };

    // A cached error is immutable and can be replayed without consulting the
    // current session map. In particular, a later session with the same numeric
    // ID must not change the original request's result.
    if let Some(
        response @ ServerMessage::AttachResult {
            outcome: AttachOutcome::Error(_),
            ..
        },
    ) = cached.clone()
    {
        let _ = client.try_deliver(response);
        return Ok(());
    }

    // This is the attach/takeover linearization boundary. Shutdown sets its
    // flag under the same server lock, so exactly one side can commit first.
    before_commit();
    let mut state = server.lock().map_err(lock_error)?;
    if state.shutting_down {
        let response = ServerMessage::AttachResult {
            request_id: *request_id,
            outcome: AttachOutcome::Error(AttachError::Failed {
                message: SHUTDOWN_ERROR_MESSAGE.to_string(),
            }),
        };
        if cached.is_some() {
            // The original successful result remains the immutable cached
            // result, but it cannot rebind while shutdown is in progress.
            drop(state);
            let _ = client.try_deliver(response);
        } else {
            let waiters = state.complete_request(key, response.clone());
            drop(state);
            deliver_to_waiters(waiters, response);
        }
        return Ok(());
    }
    let Some(pane) = state.sessions.get(session).cloned() else {
        if cached.is_some() {
            drop(state);
            return deliver_attach_error(client, *request_id, AttachError::Superseded);
        }
        let response = ServerMessage::AttachResult {
            request_id: *request_id,
            outcome: AttachOutcome::Error(AttachError::SessionNotFound { session: *session }),
        };
        let waiters = state.complete_request(key, response.clone());
        drop(state);
        deliver_to_waiters(waiters, response);
        return Ok(());
    };
    let mut pane_state = pane.lock().map_err(lock_error)?;
    if pane_state.identity != *identity {
        if cached.is_some() {
            drop(pane_state);
            drop(state);
            return deliver_attach_error(client, *request_id, AttachError::Superseded);
        }
        let response = ServerMessage::AttachResult {
            request_id: *request_id,
            outcome: AttachOutcome::Error(AttachError::IdentityMismatch {
                session: *session,
                mismatch: identity_mismatch(pane_state.identity, *identity),
            }),
        };
        let waiters = state.complete_request(key, response.clone());
        drop(pane_state);
        drop(state);
        deliver_to_waiters(waiters, response);
        return Ok(());
    }
    let recoverable_stopping =
        pane_state.lifecycle == PaneLifecycle::Stopping && !pane_state.stop_driver_active;
    if pane_state.lifecycle != PaneLifecycle::Running && !recoverable_stopping {
        if cached.is_some() {
            drop(pane_state);
            drop(state);
            return deliver_attach_error(client, *request_id, AttachError::Superseded);
        }
        let response = ServerMessage::AttachResult {
            request_id: *request_id,
            outcome: AttachOutcome::Error(AttachError::SessionNotFound { session: *session }),
        };
        let waiters = state.complete_request(key, response.clone());
        drop(pane_state);
        drop(state);
        deliver_to_waiters(waiters, response);
        return Ok(());
    }

    if let Some(response) = cached {
        let lease = match response {
            ServerMessage::AttachResult {
                outcome: AttachOutcome::Attached { lease, .. },
                ..
            } => lease,
            _ => unreachable!("cached attach response has attach shape"),
        };
        let usable = pane_state.owner.as_ref().is_some_and(|owner| {
            owner.scope == scope
                && owner.lease == lease
                && client.is_active()
                && (owner.client.id == client.id || resumed)
        });
        if !usable {
            return deliver_attach_error(client, *request_id, AttachError::Superseded);
        }
        if pane_state.lifecycle == PaneLifecycle::Running {
            if let Err(error) = pane_state.resize(*rows, *cols) {
                drop(pane_state);
                drop(state);
                return deliver_attach_error(
                    client,
                    *request_id,
                    AttachError::Failed {
                        message: error.to_string(),
                    },
                );
            }
        }
        if let Some(previous) = pane_state.owner.replace(AttachmentOwner {
            scope,
            lease,
            client: client.clone(),
        }) {
            if previous.client.id != client.id
                && !previous.client.try_deliver(ServerMessage::TakenOver {
                    pane: pane_state.pane,
                    lease: previous.lease,
                })
            {
                previous.client.disconnect();
            }
        }
        let response = ServerMessage::AttachResult {
            request_id: *request_id,
            outcome: AttachOutcome::Attached {
                session: *session,
                pane: pane_state.pane_info(),
                lease,
            },
        };
        let foreground = pane_state.refresh_foreground_process();
        // Lock rule L4: the replay below keeps the pane barrier but must not
        // keep the global lock.
        drop(state);
        before_replay();
        // An overflowed transaction leaves the attachment unreconciled, which
        // the client already handles by re-attaching. It must not disconnect
        // the connection it has just attached — that turned one full queue into
        // a reconnect loop.
        let _ = deliver_attach_transaction(
            client,
            &response,
            *request_id,
            lease,
            &pane_state,
            &foreground,
        );
        return Ok(());
    }

    if pane_state.lifecycle == PaneLifecycle::Running {
        if let Err(error) = pane_state.resize(*rows, *cols) {
            let response = ServerMessage::AttachResult {
                request_id: *request_id,
                outcome: AttachOutcome::Error(AttachError::Failed {
                    message: error.to_string(),
                }),
            };
            let waiters = state.complete_request(key, response.clone());
            drop(pane_state);
            drop(state);
            deliver_to_waiters(waiters, response);
            return Ok(());
        }
    }
    let lease = match state.allocate_lease() {
        Ok(lease) => lease,
        Err(error) => {
            let response = ServerMessage::AttachResult {
                request_id: *request_id,
                outcome: AttachOutcome::Error(AttachError::Failed {
                    message: error.to_string(),
                }),
            };
            let waiters = state.complete_request(key, response.clone());
            drop(pane_state);
            drop(state);
            deliver_to_waiters(waiters, response);
            return Ok(());
        }
    };

    if let Some(previous) = pane_state.owner.take() {
        if !previous.client.try_deliver(ServerMessage::TakenOver {
            pane: pane_state.pane,
            lease: previous.lease,
        }) {
            previous.client.disconnect();
        }
    }

    let response = ServerMessage::AttachResult {
        request_id: *request_id,
        outcome: AttachOutcome::Attached {
            session: *session,
            pane: pane_state.pane_info(),
            lease,
        },
    };
    let mut waiters = state.complete_request(key, response.clone());
    if waiters.is_empty() {
        // The initiating transport may have disconnected while the operation
        // was in flight. Preserve a logical owner so an exact resumed retry can
        // reclaim this lease instead of being confused with explicit detach.
        waiters.push(client.clone());
    }
    let selected_owner = waiters.last().cloned().expect("non-empty attach waiters");
    pane_state.owner = Some(AttachmentOwner {
        scope,
        lease,
        client: selected_owner.clone(),
    });
    let foreground = pane_state.refresh_foreground_process();
    // Lock rule L4: hand the global lock back before replay. Replay still holds
    // the pane barrier, so ordering against live output is unchanged, but an
    // attach to a busy pane no longer serializes every other pane and client.
    drop(state);
    before_replay();
    for waiter in waiters {
        let accepted = deliver_attach_transaction(
            &waiter,
            &response,
            *request_id,
            lease,
            &pane_state,
            &foreground,
        );
        // A transaction that overflowed the waiter's queue is abandoned, not
        // punished: the client re-attaches once its writer thread has caught
        // up. Disconnecting here made a momentarily-behind client reconnect in
        // a loop, and it tore down that connection's other panes too.
        if accepted && waiter.id != selected_owner.id {
            let _ = waiter.try_deliver(ServerMessage::TakenOver {
                pane: pane_state.pane,
                lease,
            });
        }
    }
    Ok(())
}

fn deliver_attach_transaction(
    client: &ClientHandle,
    response: &ServerMessage,
    request_id: RequestId,
    lease: AttachmentLease,
    pane: &PaneState,
    foreground: &ForegroundProcessInfo,
) -> bool {
    deliver_attach_transaction_with_hook(
        client,
        response,
        request_id,
        lease,
        pane,
        foreground,
        || {},
    )
}

fn deliver_attach_transaction_with_hook(
    client: &ClientHandle,
    response: &ServerMessage,
    request_id: RequestId,
    lease: AttachmentLease,
    pane: &PaneState,
    foreground: &ForegroundProcessInfo,
    after_replay_begin: impl FnOnce(),
) -> bool {
    let mut accepted = client.try_deliver(response.clone())
        && client.try_deliver(ServerMessage::ReplayBegin {
            request_id,
            pane: pane.pane,
            lease,
            first_sequence: pane.history_start,
            watermark: pane.next_output,
            omitted_prefix_bytes: pane.history_start.get(),
        });
    if accepted {
        after_replay_begin();
    }
    let mut sequence = pane.history_start;
    // The chunks are the pane's own retained history, refcounted. A queued
    // replay therefore costs one pointer per chunk rather than a second copy of
    // everything the pane is holding.
    for chunk in pane.raw_history.replay_chunks() {
        if !accepted {
            break;
        }
        let length = chunk.len();
        accepted = client.try_enqueue(ClientDelivery::Replay {
            request_id,
            pane: pane.pane,
            lease,
            sequence,
            bytes: chunk,
        });
        let Some(next) = sequence.checked_add_bytes(length) else {
            return false;
        };
        sequence = next;
    }
    accepted
        && sequence == pane.next_output
        && client.try_deliver(ServerMessage::ReplayEnd {
            request_id,
            pane: pane.pane,
            lease,
            watermark: pane.next_output,
        })
        && client.try_deliver(ServerMessage::ForegroundProcess {
            pane: pane.pane,
            lease,
            process: foreground.clone(),
        })
}

fn deliver_to_waiters(waiters: Vec<ClientHandle>, response: ServerMessage) {
    for waiter in waiters {
        if !waiter.try_deliver(response.clone()) {
            waiter.disconnect();
        }
    }
}

fn deliver_attach_error(
    client: &ClientHandle,
    request_id: RequestId,
    error: AttachError,
) -> io::Result<()> {
    let _ = client.try_deliver(ServerMessage::AttachResult {
        request_id,
        outcome: AttachOutcome::Error(error),
    });
    Ok(())
}

/// Routes `Input`/`Paste` to a pane's PTY.
///
/// Lock rule L3 is the whole point of the shape here: the server and pane
/// mutexes cover only the O(1) lease validation — which keeps validation
/// linearized against shutdown and takeover exactly as before — and are both
/// released before a single byte is handed to the pane's writer thread. The
/// blocking `write_all`/`flush` used to run under both, so a child that stopped
/// reading its stdin froze every pane and every client in the daemon.
fn handle_leased_input(
    server: &SharedServer,
    scope: ClientScopeId,
    client: &ClientHandle,
    pane_id: PaneId,
    lease: AttachmentLease,
    bytes: Vec<u8>,
    operation: LeaseOperation,
) {
    let (writes, pane, scheduled) = {
        let state = match server.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        let Some(pane) = state.pane_by_id(pane_id) else {
            drop(state);
            reject_lease(
                client,
                pane_id,
                lease,
                operation,
                LeaseRejectionReason::PaneMissing,
            );
            return;
        };
        let pane_state = match pane.lock() {
            Ok(pane_state) => pane_state,
            Err(_) => return,
        };
        if let Err(reason) =
            pane_state.validate_mutation_lease(state.shutting_down, scope, client.id, lease)
        {
            drop(pane_state);
            drop(state);
            reject_lease(client, pane_id, lease, operation, reason);
            return;
        }
        // Ordinary mutations never change the connection binding. Only a fresh
        // attach or an exact cached attach on a resumed scope may do that.
        let writes = Arc::clone(&pane_state.writes);
        let scheduled = Arc::clone(&pane_state.foreground_poll_scheduled);
        drop(pane_state);
        (writes, pane, scheduled)
    };

    let schedule = input_may_change_foreground(&bytes);
    if let Err(refusal) = writes.enqueue(bytes) {
        // The bytes were definitely not delivered, so say so. `LeaseRejected`
        // is the pane-scoped refusal channel: it does not close the connection,
        // and the client treats it as conclusive rather than assuming the
        // keystrokes landed. `NotOwner` is the daemon's established "this
        // mutation is refused right now" reason — `validate_mutation_lease`
        // already returns it for shutdown and for a stopping pane. A dedicated
        // reason would be clearer but is a wire change; see BACKLOG F8.
        eprintln!(
            "refusing PTY input for pane {}: writer queue {refusal:?}",
            pane_id.0
        );
        reject_lease(
            client,
            pane_id,
            lease,
            operation,
            LeaseRejectionReason::NotOwner,
        );
        return;
    }
    if schedule {
        schedule_foreground_process_poll(pane, scheduled);
    }
}

fn handle_leased_resize(
    server: &SharedServer,
    scope: ClientScopeId,
    client: &ClientHandle,
    pane_id: PaneId,
    lease: AttachmentLease,
    rows: u16,
    cols: u16,
) {
    let state = match server.lock() {
        Ok(state) => state,
        Err(_) => return,
    };
    let Some(pane) = state.pane_by_id(pane_id) else {
        reject_lease(
            client,
            pane_id,
            lease,
            LeaseOperation::Resize,
            LeaseRejectionReason::PaneMissing,
        );
        return;
    };
    let mut pane_state = match pane.lock() {
        Ok(pane) => pane,
        Err(_) => return,
    };
    if let Err(reason) =
        pane_state.validate_mutation_lease(state.shutting_down, scope, client.id, lease)
    {
        reject_lease(client, pane_id, lease, LeaseOperation::Resize, reason);
        return;
    }
    if let Err(error) = pane_state.resize(rows, cols) {
        eprintln!("failed to resize pane {}: {error}", pane_id.0);
    }
}

fn handle_leased_detach(
    server: &SharedServer,
    scope: ClientScopeId,
    client: &ClientHandle,
    pane_id: PaneId,
    lease: AttachmentLease,
) {
    let state = match server.lock() {
        Ok(state) => state,
        Err(_) => return,
    };
    let Some(pane) = state.pane_by_id(pane_id) else {
        reject_lease(
            client,
            pane_id,
            lease,
            LeaseOperation::Detach,
            LeaseRejectionReason::PaneMissing,
        );
        return;
    };
    let mut pane_state = match pane.lock() {
        Ok(pane) => pane,
        Err(_) => return,
    };
    if let Err(reason) =
        pane_state.validate_mutation_lease(state.shutting_down, scope, client.id, lease)
    {
        reject_lease(client, pane_id, lease, LeaseOperation::Detach, reason);
        return;
    }
    pane_state.owner = None;
}

fn reject_lease(
    client: &ClientHandle,
    pane: PaneId,
    lease: AttachmentLease,
    operation: LeaseOperation,
    reason: LeaseRejectionReason,
) {
    let _ = client.try_deliver(ServerMessage::LeaseRejected {
        pane,
        lease,
        operation,
        reason,
    });
}

fn handle_stop_request(
    server: &SharedServer,
    scope: ClientScopeId,
    client: &ClientHandle,
    request: ClientMessage,
) -> io::Result<()> {
    let ClientMessage::Stop {
        request_id,
        identity,
        pane,
        lease,
    } = &request
    else {
        unreachable!();
    };
    match server
        .lock()
        .map_err(lock_error)?
        .begin_request(scope, &request, client)
    {
        RequestDisposition::Cached(response) => {
            let _ = client.try_deliver(response);
            return Ok(());
        }
        RequestDisposition::Pending => return Ok(()),
        RequestDisposition::Collision => {
            return deliver_stop_error(client, *request_id, StopError::RequestCollision)
        }
        RequestDisposition::Expired => {
            return deliver_stop_error(client, *request_id, StopError::RetryExpired)
        }
        RequestDisposition::TooManyPending => {
            let response = ServerMessage::StopResult {
                request_id: *request_id,
                outcome: StopOutcome::Error(StopError::Failed {
                    message: "too many pending requests".to_string(),
                }),
            };
            complete_and_deliver(
                server,
                RequestKey {
                    scope,
                    request_id: *request_id,
                },
                response,
            );
            return Ok(());
        }
        RequestDisposition::New => {}
    }

    let key = RequestKey {
        scope,
        request_id: *request_id,
    };
    let target = server.lock().map_err(lock_error)?.pane_by_id(*pane);
    let Some(target) = target else {
        complete_and_deliver(
            server,
            key,
            ServerMessage::StopResult {
                request_id: *request_id,
                outcome: StopOutcome::AlreadyAbsent,
            },
        );
        return Ok(());
    };
    {
        let mut pane_state = target.lock().map_err(lock_error)?;
        if pane_state.identity != *identity {
            let mismatch = identity_mismatch(pane_state.identity, *identity);
            drop(pane_state);
            complete_and_deliver(
                server,
                key,
                ServerMessage::StopResult {
                    request_id: *request_id,
                    outcome: StopOutcome::Error(StopError::IdentityMismatch {
                        pane: *pane,
                        mismatch,
                    }),
                },
            );
            return Ok(());
        }
        if matches!(
            pane_state.lifecycle,
            PaneLifecycle::Exited | PaneLifecycle::Removed
        ) {
            drop(pane_state);
            complete_and_deliver(
                server,
                key,
                ServerMessage::StopResult {
                    request_id: *request_id,
                    outcome: StopOutcome::AlreadyAbsent,
                },
            );
            return Ok(());
        }
        if let Err(reason) = pane_state.validate_stop_lease(scope, client.id, *lease) {
            drop(pane_state);
            complete_and_deliver(
                server,
                key,
                ServerMessage::StopResult {
                    request_id: *request_id,
                    outcome: StopOutcome::Error(StopError::LeaseRejected(reason)),
                },
            );
            return Ok(());
        }
        pane_state.pending_stops.push(key);
        if pane_state.lifecycle == PaneLifecycle::Running {
            pane_state.lifecycle = PaneLifecycle::Stopping;
        }
    }
    start_stop_if_needed(Arc::clone(server), target);
    Ok(())
}

fn handle_agent_status_request(
    server: &SharedServer,
    scope: ClientScopeId,
    client: &ClientHandle,
    request: ClientMessage,
) -> io::Result<()> {
    let request_id = request_id(&request).expect("agent status request is correlated");
    match server
        .lock()
        .map_err(lock_error)?
        .begin_request(scope, &request, client)
    {
        RequestDisposition::Cached(response) => {
            let _ = client.try_deliver(response);
            return Ok(());
        }
        RequestDisposition::Pending => return Ok(()),
        RequestDisposition::Collision => {
            return deliver_agent_status_error(
                client,
                request_id,
                AgentStatusError::RequestCollision,
            );
        }
        RequestDisposition::Expired => {
            return deliver_agent_status_error(client, request_id, AgentStatusError::RetryExpired);
        }
        RequestDisposition::TooManyPending => {
            complete_and_deliver(
                server,
                RequestKey { scope, request_id },
                ServerMessage::AgentStatusResult {
                    request_id,
                    outcome: AgentStatusOutcome::Error(AgentStatusError::Failed {
                        message: "too many pending requests".to_string(),
                    }),
                },
            );
            return Ok(());
        }
        RequestDisposition::New => {}
    }

    let outcome = {
        let mut state = server.lock().map_err(lock_error)?;
        match request {
            ClientMessage::UpdateAgentStatus { record, .. } => {
                update_agent_status(&mut state, record)
            }
            ClientMessage::GetAgentStatus { query, .. } => query_agent_status(&state, query),
            _ => unreachable!("agent status handler received another request"),
        }
    };
    complete_and_deliver(
        server,
        RequestKey { scope, request_id },
        ServerMessage::AgentStatusResult {
            request_id,
            outcome,
        },
    );
    Ok(())
}

fn update_agent_status(state: &mut ServerState, record: AgentStatusRecord) -> AgentStatusOutcome {
    if record.schema_version != AGENT_STATUS_SCHEMA_VERSION {
        return AgentStatusOutcome::Error(AgentStatusError::WrongSchema {
            expected: AGENT_STATUS_SCHEMA_VERSION,
            received: record.schema_version,
        });
    }
    let missing = missing_agent_identity_error(state, record.identity, record.chat_id);
    let Some(agent_state) = state.agent_states.get_mut(&record.identity) else {
        return AgentStatusOutcome::Error(missing);
    };
    if record.chat_id != agent_state.metadata.chat_id {
        return AgentStatusOutcome::Error(AgentStatusError::WrongChat {
            expected: agent_state.metadata.chat_id,
            received: record.chat_id,
        });
    }
    if record.agent != agent_state.metadata.agent {
        return AgentStatusOutcome::Error(AgentStatusError::WrongAgent {
            expected: agent_state.metadata.agent,
            received: record.agent,
        });
    }
    if record.generation != agent_state.metadata.generation {
        return AgentStatusOutcome::Error(AgentStatusError::StaleGeneration {
            current: agent_state.metadata.generation,
            received: record.generation,
        });
    }
    if let Some(current) = agent_state
        .status
        .filter(|current| current.status.is_final())
    {
        if current.status != record.status {
            return AgentStatusOutcome::Error(AgentStatusError::FinalStatusConflict {
                current: current.status,
                attempted: record.status,
            });
        }
        return AgentStatusOutcome::Updated(current);
    }
    agent_state.status = Some(record);
    AgentStatusOutcome::Updated(record)
}

fn query_agent_status(state: &ServerState, query: AgentStatusQuery) -> AgentStatusOutcome {
    if query.schema_version != AGENT_STATUS_SCHEMA_VERSION {
        return AgentStatusOutcome::Error(AgentStatusError::WrongSchema {
            expected: AGENT_STATUS_SCHEMA_VERSION,
            received: query.schema_version,
        });
    }
    let Some(agent_state) = state.agent_states.get(&query.identity) else {
        return AgentStatusOutcome::Error(missing_agent_identity_error(
            state,
            query.identity,
            query.chat_id,
        ));
    };
    if query.chat_id != agent_state.metadata.chat_id {
        return AgentStatusOutcome::Error(AgentStatusError::WrongChat {
            expected: agent_state.metadata.chat_id,
            received: query.chat_id,
        });
    }
    if query.agent != agent_state.metadata.agent {
        return AgentStatusOutcome::Error(AgentStatusError::WrongAgent {
            expected: agent_state.metadata.agent,
            received: query.agent,
        });
    }
    if query.generation != agent_state.metadata.generation {
        return AgentStatusOutcome::Error(AgentStatusError::StaleGeneration {
            current: agent_state.metadata.generation,
            received: query.generation,
        });
    }
    AgentStatusOutcome::Current(agent_state.status)
}

fn missing_agent_identity_error(
    state: &ServerState,
    identity: SessionIdentity,
    chat_id: u64,
) -> AgentStatusError {
    if state.session_by_identity.contains_key(&identity) {
        return AgentStatusError::NotAgentSession { identity };
    }
    if state
        .agent_states
        .keys()
        .any(|candidate| candidate.token == identity.token)
    {
        return AgentStatusError::IdentityMismatch(IdentityMismatch::Namespace);
    }
    if state.agent_states.iter().any(|(candidate, agent)| {
        candidate.namespace == identity.namespace && agent.metadata.chat_id == chat_id
    }) {
        return AgentStatusError::IdentityMismatch(IdentityMismatch::SessionToken);
    }
    AgentStatusError::SessionNotFound { identity }
}

fn deliver_agent_status_error(
    client: &ClientHandle,
    request_id: RequestId,
    error: AgentStatusError,
) -> io::Result<()> {
    let _ = client.try_deliver(ServerMessage::AgentStatusResult {
        request_id,
        outcome: AgentStatusOutcome::Error(error),
    });
    Ok(())
}

fn deliver_stop_error(
    client: &ClientHandle,
    request_id: RequestId,
    error: StopError,
) -> io::Result<()> {
    let _ = client.try_deliver(ServerMessage::StopResult {
        request_id,
        outcome: StopOutcome::Error(error),
    });
    Ok(())
}

fn complete_and_deliver(server: &SharedServer, key: RequestKey, response: ServerMessage) {
    let waiters = server
        .lock()
        .map(|mut state| state.complete_request(key, response.clone()))
        .unwrap_or_default();
    for waiter in waiters {
        if !waiter.try_deliver(response.clone()) {
            waiter.disconnect();
        }
    }
}

fn read_client_message(stream: &mut UnixStream) -> io::Result<Option<ClientMessage>> {
    match read_message::<ClientMessage>(stream) {
        Ok(message) => Ok(Some(message)),
        Err(error) if is_client_disconnect(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_client_disconnect(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    )
}

fn create_session(server: &SharedServer, spec: SessionCreateSpec) -> io::Result<SharedPane> {
    create_session_with_spawner(server, spec, spawn_pane)
}

fn create_session_with_spawner(
    server: &SharedServer,
    spec: SessionCreateSpec,
    spawn: impl FnOnce(SessionId, PaneSpawnSpec) -> io::Result<SpawnedPane>,
) -> io::Result<SharedPane> {
    let identity = spec.pane.identity;
    let session = {
        let mut state = server.lock().map_err(lock_error)?;
        state.reserve_session(spec.requested_id, identity)?
    };
    let mut spawned = match spawn(session, spec.pane) {
        Ok(spawned) => spawned,
        Err(error) => {
            if let Ok(mut state) = server.lock() {
                state.release_session_reservation(session, identity);
            }
            return Err(error);
        }
    };

    {
        let publish = server
            .lock()
            .map_err(lock_error)?
            .publish_reserved_session(session, Arc::clone(&spawned.pane));
        if let Err(error) = publish {
            let unpublished = spawned.pane.lock().ok().map(|pane| {
                // Retire the writer thread with the child it would have fed.
                pane.writes.close();
                pane.process_group
            });
            cleanup_unpublished_child(&mut spawned.child, unpublished);
            if let Ok(mut state) = server.lock() {
                state.release_session_reservation(session, identity);
            }
            return Err(error);
        }
    }
    spawn_reader(
        spawned.reader,
        Arc::clone(&spawned.pane),
        Arc::clone(server),
    );
    spawn_waiter(spawned.child, Arc::clone(&spawned.pane), Arc::clone(server));
    Ok(spawned.pane)
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, pane: SharedPane, server: SharedServer) {
    thread::spawn(move || {
        let Some(scheduled) = pane
            .lock()
            .ok()
            .map(|pane_state| Arc::clone(&pane_state.foreground_poll_scheduled))
        else {
            return;
        };
        let mut buffer = [0; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => match handle_pty_read(&pane, &scheduled, Arc::from(&buffer[..n])) {
                    Ok(true) => {
                        start_foreground_process_poll(Arc::clone(&pane), Arc::clone(&scheduled))
                    }
                    Ok(false) => {}
                    Err(error) => {
                        eprintln!("failed to sequence PTY output: {error}");
                        break;
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    eprintln!("failed to read PTY output: {error}");
                    break;
                }
            }
        }
        if let Ok(mut pane_state) = pane.lock() {
            pane_state.reader_done = true;
            pane_state.notify_lifecycle();
        }
        maybe_finalize(&server, &pane);
    });
}

/// Publishes one PTY read and reports whether it newly claimed the debounced
/// foreground-process poll.
///
/// The reader used to call `broadcast_foreground_process_if_changed` after
/// **every** 8 KiB read: a pane lock, a master lock, a `tcgetpgrp` ioctl and a
/// `/proc/<pid>/cmdline` read per read, on a path that runs at whatever rate
/// the child produces output. It now shares the same debounced poll the input
/// path already used, so a chatty pane probes at most once per debounce window.
fn handle_pty_read(
    pane: &SharedPane,
    scheduled: &AtomicBool,
    bytes: Arc<[u8]>,
) -> io::Result<bool> {
    publish_pty_output(pane, bytes)?;
    Ok(claim_foreground_process_poll(scheduled))
}

fn publish_pty_output(pane: &SharedPane, bytes: Arc<[u8]>) -> io::Result<()> {
    publish_pty_output_with_hook(pane, bytes, || {})
}

fn publish_pty_output_with_hook(
    pane: &SharedPane,
    bytes: Arc<[u8]>,
    before_pane_lock: impl FnOnce(),
) -> io::Result<()> {
    // The test hook makes contention against the same pane barrier observable
    // without changing production synchronization or relying on a sleep.
    before_pane_lock();
    let mut pane_state = pane.lock().map_err(lock_error)?;
    let sequence = pane_state.append_raw_history(&bytes)?;
    if let Some(owner) = pane_state.owner.clone() {
        // The retained history chunk and the queued frame are the same
        // allocation, so a backed-up client no longer doubles the pane's
        // resident output.
        if !owner.client.try_enqueue(ClientDelivery::Output {
            pane: pane_state.pane,
            lease: owner.lease,
            sequence,
            bytes,
        }) {
            // Delivery failure invalidates only this transport. Keep the
            // logical (scope, lease) owner dormant for an exact resumed retry.
            owner.client.disconnect();
        }
    }
    Ok(())
}

fn spawn_waiter(mut child: Box<dyn Child + Send + Sync>, pane: SharedPane, server: SharedServer) {
    thread::spawn(move || loop {
        match child.wait() {
            Ok(status) => {
                if let Ok(mut pane_state) = pane.lock() {
                    pane_state.child_exit = Some(exit_info(status));
                    pane_state.last_wait_error = None;
                    pane_state.notify_lifecycle();
                }
                maybe_finalize(&server, &pane);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                let message = error.to_string();
                record_wait_failure(&pane, &message);
                fail_pending_stops(
                    &server,
                    &pane,
                    format!("failed to wait for PTY child: {message}"),
                );
                let signal = pane
                    .lock()
                    .ok()
                    .map(|pane| Arc::clone(&pane.lifecycle_signal));
                if let Some(signal) = signal {
                    let (lock, ready) = &*signal;
                    if let Ok(generation) = lock.lock() {
                        let _ = ready.wait_timeout(generation, WAIT_RETRY_DELAY);
                    }
                }
            }
        }
    });
}

fn record_wait_failure(pane: &SharedPane, message: &str) {
    if let Ok(mut pane_state) = pane.lock() {
        pane_state.last_wait_error = Some(message.to_string());
        // The termination thread exclusively owns stop_driver_active. Clearing
        // it here could launch a duplicate TERM/KILL driver while that thread
        // still waits.
        pane_state.notify_lifecycle();
    }
}

fn maybe_finalize(server: &SharedServer, pane: &SharedPane) {
    let (owner, exit, pending, deliveries) = {
        let mut state = match server.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        let mut pane_state = match pane.lock() {
            Ok(pane) => pane,
            Err(_) => return,
        };
        if pane_state.lifecycle == PaneLifecycle::Removed
            || !pane_state.reader_done
            || pane_state.child_exit.is_none()
        {
            return;
        }
        pane_state.lifecycle = PaneLifecycle::Exited;
        let exit = pane_state.child_exit.clone().expect("checked exit");
        let session = pane_state.session;
        if let Some(metadata) = pane_state.agent {
            state.record_agent_exit(pane_state.identity, metadata, &exit);
        }
        state.remove_session_if_same(session, pane);
        pane_state.lifecycle = PaneLifecycle::Removed;
        pane_state.stop_driver_active = false;
        // Retire the writer thread with the pane. `close` only touches the
        // queue's own leaf mutex, so it is safe under the pane lock.
        pane_state.writes.close();
        let owner = pane_state.owner.take();
        let pending = std::mem::take(&mut pane_state.pending_stops);
        pane_state.notify_lifecycle();
        let mut deliveries = Vec::new();
        for key in &pending {
            let response = ServerMessage::StopResult {
                request_id: key.request_id,
                outcome: StopOutcome::Stopped { exit: exit.clone() },
            };
            let waiters = state.complete_request(*key, response.clone());
            deliveries.push((waiters, response));
        }
        (owner, exit, pending, deliveries)
    };

    if let Some(owner) = owner {
        if !owner.client.try_deliver(ServerMessage::PaneExited {
            pane: pane.lock().ok().map(|pane| pane.pane).unwrap_or(PaneId(0)),
            lease: owner.lease,
            exit: exit.clone(),
        }) {
            owner.client.disconnect();
        }
    }
    for (waiters, response) in deliveries {
        for waiter in waiters {
            if !waiter.try_deliver(response.clone()) {
                waiter.disconnect();
            }
        }
    }
    let _ = pending;
}

fn start_stop_if_needed(server: SharedServer, pane: SharedPane) {
    let groups = {
        let mut pane_state = match pane.lock() {
            Ok(pane) => pane,
            Err(_) => return,
        };
        if matches!(
            pane_state.lifecycle,
            PaneLifecycle::Exited | PaneLifecycle::Removed
        ) || pane_state.stop_driver_active
        {
            return;
        }
        pane_state.lifecycle = PaneLifecycle::Stopping;
        pane_state.stop_driver_active = true;
        let foreground = pane_state
            .master
            .lock()
            .ok()
            .and_then(|master| master.process_group_leader())
            .filter(|group| *group > 1 && *group != pane_state.process_group);
        (pane_state.process_group, foreground)
    };

    thread::spawn(move || {
        let result = drive_termination(
            |signal| signal_process_groups(groups, signal),
            |timeout| wait_for_finalization(&pane, timeout),
        );
        let (restore_running, message) = match result {
            TerminationResult::Finalized => return,
            TerminationResult::TermFailed(error) => {
                (true, format!("failed to signal PTY group: {error}"))
            }
            TerminationResult::KillFailed(error) => {
                (false, format!("failed to kill PTY group: {error}"))
            }
            TerminationResult::TimedOut => (
                false,
                "timed out waiting for PTY child reap and output drain".to_string(),
            ),
        };
        if let Ok(mut pane_state) = pane.lock() {
            pane_state.stop_driver_active = false;
            if restore_running && pane_state.lifecycle == PaneLifecycle::Stopping {
                pane_state.lifecycle = PaneLifecycle::Running;
            }
            pane_state.notify_lifecycle();
        }
        fail_pending_stops(&server, &pane, message);
    });
}

#[derive(Debug)]
enum TerminationResult {
    Finalized,
    TermFailed(io::Error),
    KillFailed(io::Error),
    TimedOut,
}

fn drive_termination(
    mut signal: impl FnMut(libc::c_int) -> io::Result<()>,
    mut wait: impl FnMut(Duration) -> bool,
) -> TerminationResult {
    if let Err(error) = signal(libc::SIGTERM) {
        return TerminationResult::TermFailed(error);
    }
    if wait(STOP_TERM_GRACE) {
        return TerminationResult::Finalized;
    }
    if let Err(error) = signal(libc::SIGKILL) {
        return TerminationResult::KillFailed(error);
    }
    if wait(STOP_FINALIZE_TIMEOUT) {
        TerminationResult::Finalized
    } else {
        TerminationResult::TimedOut
    }
}

fn wait_for_finalization(pane: &SharedPane, timeout: Duration) -> bool {
    let signal = match pane.lock() {
        Ok(pane_state) => {
            if pane_state.lifecycle == PaneLifecycle::Removed {
                return true;
            }
            Arc::clone(&pane_state.lifecycle_signal)
        }
        Err(_) => return false,
    };
    let deadline = Instant::now() + timeout;
    let (lock, ready) = &*signal;
    let mut observed = match lock.lock() {
        Ok(generation) => *generation,
        Err(_) => return false,
    };
    loop {
        // Never hold the condition-variable mutex while taking the pane mutex:
        // lifecycle writers notify while holding the pane mutex.
        if pane
            .lock()
            .ok()
            .is_some_and(|pane_state| pane_state.lifecycle == PaneLifecycle::Removed)
        {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let generation = match lock.lock() {
            Ok(generation) => generation,
            Err(_) => return false,
        };
        if *generation != observed {
            observed = *generation;
            continue;
        }
        let Ok((generation, timed)) = ready.wait_timeout(generation, remaining) else {
            return false;
        };
        let changed = *generation != observed;
        observed = *generation;
        if timed.timed_out() && !changed {
            return false;
        }
    }
}

fn fail_pending_stops(server: &SharedServer, pane: &SharedPane, message: String) {
    let deliveries = {
        let mut state = match server.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        let mut pane_state = match pane.lock() {
            Ok(pane) => pane,
            Err(_) => return,
        };
        let pending = std::mem::take(&mut pane_state.pending_stops);
        pending
            .into_iter()
            .map(|key| {
                let response = ServerMessage::StopResult {
                    request_id: key.request_id,
                    outcome: StopOutcome::Error(StopError::Failed {
                        message: message.clone(),
                    }),
                };
                let waiters = state.complete_request(key, response.clone());
                (waiters, response)
            })
            .collect::<Vec<_>>()
    };
    for (waiters, response) in deliveries {
        for waiter in waiters {
            let _ = waiter.try_deliver(response.clone());
        }
    }
}

fn signal_process_groups(
    (root, foreground): (libc::pid_t, Option<libc::pid_t>),
    signal: libc::c_int,
) -> io::Result<()> {
    signal_process_group(root, signal)?;
    if let Some(foreground) = foreground {
        // The stable child group is authoritative. The foreground group is a
        // best-effort supplement for interactive job control; failure there
        // must not turn an already-delivered root signal into a false
        // "nothing was signalled" rollback.
        let _ = signal_process_group(foreground, signal);
    }
    Ok(())
}

fn signal_process_group(group: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    let own_group = unsafe { libc::getpgrp() };
    if group <= 1 || group == own_group {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to signal unsafe process group {group}"),
        ));
    }
    if unsafe { libc::kill(-group, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn exit_info(status: ExitStatus) -> ExitInfo {
    ExitInfo {
        code: status.exit_code(),
        signal: status.signal().map(ToOwned::to_owned),
    }
}

fn broadcast_foreground_process_if_changed(pane: &SharedPane) {
    let mut pane_state = match pane.lock() {
        Ok(pane) => pane,
        Err(_) => return,
    };
    let Some(process) = pane_state.refresh_foreground_process_if_changed() else {
        return;
    };
    deliver_foreground_process(&pane_state, process);
}

fn deliver_foreground_process(pane: &PaneState, process: ForegroundProcessInfo) {
    let Some(owner) = pane.owner.clone() else {
        return;
    };
    if !owner.client.try_deliver(ServerMessage::ForegroundProcess {
        pane: pane.pane,
        lease: owner.lease,
        process,
    }) {
        // As with output delivery, retain logical ownership while making the
        // failed connection immediately unusable for mutations.
        owner.client.disconnect();
    }
}

/// Claims the debounce slot, returning whether this caller now owns a poll.
fn claim_foreground_process_poll(scheduled: &AtomicBool) -> bool {
    !scheduled.swap(true, Ordering::AcqRel)
}

fn schedule_foreground_process_poll(pane: SharedPane, scheduled: Arc<AtomicBool>) {
    if claim_foreground_process_poll(&scheduled) {
        start_foreground_process_poll(pane, scheduled);
    }
}

/// Runs the debounced poll. The caller must already own the debounce slot.
fn start_foreground_process_poll(pane: SharedPane, scheduled: Arc<AtomicBool>) {
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

impl PaneState {
    fn session_info(&self) -> SessionInfo {
        SessionInfo {
            id: self.session,
            identity: self.identity,
            name: self.name.clone(),
            pane: self.pane,
            attached: self.owner.is_some(),
        }
    }

    fn pane_info(&self) -> PaneInfo {
        PaneInfo {
            id: self.pane,
            title: self.title.clone(),
            rows: self.rows,
            cols: self.cols,
        }
    }

    fn validate_lease(
        &self,
        scope: ClientScopeId,
        client_id: ClientId,
        lease: AttachmentLease,
    ) -> Result<(), LeaseRejectionReason> {
        let Some(owner) = &self.owner else {
            return Err(LeaseRejectionReason::NotOwner);
        };
        if owner.lease != lease {
            return Err(LeaseRejectionReason::StaleLease);
        }
        if owner.scope != scope || owner.client.id != client_id || !owner.client.is_active() {
            return Err(LeaseRejectionReason::NotOwner);
        }
        Ok(())
    }

    fn validate_mutation_lease(
        &self,
        shutting_down: bool,
        scope: ClientScopeId,
        client_id: ClientId,
        lease: AttachmentLease,
    ) -> Result<(), LeaseRejectionReason> {
        if shutting_down || self.lifecycle != PaneLifecycle::Running {
            return Err(LeaseRejectionReason::NotOwner);
        }
        self.validate_lease(scope, client_id, lease)
    }

    fn validate_stop_lease(
        &self,
        scope: ClientScopeId,
        client_id: ClientId,
        lease: AttachmentLease,
    ) -> Result<(), LeaseRejectionReason> {
        // Exact pending/cached retries return before this validation. Every new
        // stop request must come from the active owning connection.
        self.validate_lease(scope, client_id, lease)
    }

    fn refresh_foreground_process(&mut self) -> ForegroundProcessInfo {
        let process = self.current_foreground_process();
        self.foreground_process = process.clone();
        process
    }

    fn refresh_foreground_process_if_changed(&mut self) -> Option<ForegroundProcessInfo> {
        let process = self.current_foreground_process();
        if process == self.foreground_process {
            None
        } else {
            self.foreground_process = process.clone();
            Some(process)
        }
    }

    fn current_foreground_process(&self) -> ForegroundProcessInfo {
        let foreground_pid = self
            .master
            .lock()
            .ok()
            .and_then(|master| master.process_group_leader())
            .and_then(|pid| u32::try_from(pid).ok());
        ForegroundProcessInfo {
            root_pid: Some(self.child_pid),
            foreground_pid,
            command: foreground_pid.and_then(command_line_for_pid),
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        let (rows, cols) = bounded_pty_dimensions(rows, cols);
        self.master
            .lock()
            .map_err(|_| io::Error::other("PTY master lock poisoned"))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(error_to_io)?;
        self.rows = rows;
        self.cols = cols;
        Ok(())
    }

    fn append_raw_history(&mut self, bytes: &Arc<[u8]>) -> io::Result<OutputSequence> {
        self.append_raw_history_with_limit(bytes, RAW_HISTORY_MAX_BYTES)
    }

    /// Appends one PTY read to the retained history and advances the sequence
    /// counters.
    ///
    /// Trimming is O(bytes dropped), not O(history): `RawHistory` releases whole
    /// chunks instead of memmoving the retained suffix down a flat `Vec`.
    fn append_raw_history_with_limit(
        &mut self,
        bytes: &Arc<[u8]>,
        limit: usize,
    ) -> io::Result<OutputSequence> {
        let sequence = self.next_output;
        self.next_output = self
            .next_output
            .checked_add_bytes(bytes.len())
            .ok_or_else(|| io::Error::other("PTY output sequence exhausted"))?;
        let dropped = self.raw_history.append(bytes, limit);
        if dropped > 0 {
            self.history_start = self
                .history_start
                .checked_add_bytes(dropped)
                .ok_or_else(|| io::Error::other("PTY history sequence exhausted"))?;
        }
        Ok(sequence)
    }

    fn notify_lifecycle(&self) {
        let (lock, ready) = &*self.lifecycle_signal;
        if let Ok(mut generation) = lock.lock() {
            *generation = generation.wrapping_add(1);
            ready.notify_all();
        }
    }
}

fn bounded_pty_dimensions(rows: u16, cols: u16) -> (u16, u16) {
    bounded_screen_dimensions(rows.max(1), cols.max(1))
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
            .map(shell_display_arg)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn shell_display_arg(arg: String) -> String {
    if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
    {
        arg
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn shell_command_args(command: String) -> Vec<String> {
    // TerminalLaunch::Command and both chat-agent commands are intentionally
    // evaluated by the login shell. Keep this distinct from MULT_AGENT_CMD's
    // client-side argv parser.
    vec!["-lc".to_string(), command]
}

fn pane_title(shell: &str, launch: &LaunchSpec) -> String {
    match launch {
        LaunchSpec::Shell => shell.to_string(),
        LaunchSpec::Command(command) => command.clone(),
    }
}

fn default_shell() -> String {
    env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("server lock poisoned")
}

fn error_to_io(error: anyhow::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    const TEST_IO_TIMEOUT: Duration = Duration::from_secs(2);

    fn request_id(value: u64) -> RequestId {
        RequestId::new(value).expect("non-zero request ID")
    }

    /// One PTY read's worth of bytes, in the refcounted shape the reader
    /// thread produces.
    fn shared(bytes: &[u8]) -> Arc<[u8]> {
        Arc::from(bytes)
    }

    struct TestClientReceiver(mpsc::Receiver<ClientDelivery>);

    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl TestClientReceiver {
        fn recv_timeout(&self, timeout: Duration) -> Result<ServerMessage, mpsc::RecvTimeoutError> {
            // Decode exactly as the client's writer thread would, so tests see
            // the wire messages rather than the queue's internal shape.
            delivery_message(self.0.recv_timeout(timeout)?)
                .ok_or(mpsc::RecvTimeoutError::Disconnected)
        }

        fn recv_delivery_timeout(
            &self,
            timeout: Duration,
        ) -> Result<ClientDelivery, mpsc::RecvTimeoutError> {
            self.0.recv_timeout(timeout)
        }

        fn try_recv(&self) -> Result<ServerMessage, mpsc::TryRecvError> {
            delivery_message(self.0.try_recv()?).ok_or(mpsc::TryRecvError::Disconnected)
        }
    }

    fn test_client(id: ClientId) -> (ClientHandle, TestClientReceiver) {
        test_client_with_capacity(id, 32)
    }

    fn test_client_with_capacity(
        id: ClientId,
        capacity: usize,
    ) -> (ClientHandle, TestClientReceiver) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let (stream, _peer) = UnixStream::pair().expect("socket pair");
        (
            ClientHandle {
                id,
                sender,
                stream: Arc::new(stream),
                active: Arc::new(AtomicBool::new(true)),
            },
            TestClientReceiver(receiver),
        )
    }

    fn test_identity() -> SessionIdentity {
        test_identity_bytes(0x41, 0x42)
    }

    fn test_identity_bytes(namespace: u8, token: u8) -> SessionIdentity {
        SessionIdentity {
            namespace: StateNamespace::from_bytes([namespace; 16]).unwrap(),
            token: mult_protocol::SessionToken::from_bytes([token; 16]).unwrap(),
        }
    }

    fn test_agent_metadata() -> AgentSessionMetadata {
        AgentSessionMetadata {
            schema_version: AGENT_STATUS_SCHEMA_VERSION,
            chat_id: 7,
            agent: mult_protocol::AgentKind::Pi,
            generation: mult_protocol::AgentGeneration::from_bytes([0x43; 16]).unwrap(),
        }
    }

    fn create_request(id: RequestId, name: &str) -> ClientMessage {
        ClientMessage::CreateSession {
            request_id: id,
            identity: test_identity(),
            requested_id: Some(SessionId(7)),
            agent: None,
            name: name.to_string(),
            cwd: None,
            env: BTreeMap::new(),
            launch: LaunchSpec::Shell,
            rows: 24,
            cols: 80,
        }
    }

    fn attach_request(id: RequestId, rows: u16, cols: u16) -> ClientMessage {
        ClientMessage::Attach {
            request_id: id,
            identity: test_identity(),
            session: SessionId(1),
            rows,
            cols,
        }
    }

    fn receive_empty_attach_transaction(
        receiver: &TestClientReceiver,
        expected_request: RequestId,
    ) -> AttachmentLease {
        let lease = match receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap() {
            ServerMessage::AttachResult {
                request_id,
                outcome:
                    AttachOutcome::Attached {
                        session: SessionId(1),
                        pane,
                        lease,
                    },
            } if request_id == expected_request && pane.id == PaneId(1) => lease,
            message => panic!("unexpected attach acknowledgement: {message:?}"),
        };
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayBegin {
                request_id,
                pane: PaneId(1),
                lease: replay_lease,
                first_sequence: OutputSequence::ZERO,
                watermark: OutputSequence::ZERO,
                omitted_prefix_bytes: 0,
            } if request_id == expected_request && replay_lease == lease
        ));
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayEnd {
                request_id,
                pane: PaneId(1),
                lease: replay_lease,
                watermark: OutputSequence::ZERO,
            } if request_id == expected_request && replay_lease == lease
        ));
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ForegroundProcess {
                pane: PaneId(1),
                lease: foreground_lease,
                ..
            } if foreground_lease == lease
        ));
        lease
    }

    fn fill_client_queue(client: &ClientHandle, capacity: usize) {
        for _ in 0..capacity {
            assert!(client.try_deliver(ServerMessage::Sessions {
                namespace: test_identity().namespace,
                sessions: Vec::new(),
            }));
        }
    }

    #[test]
    fn peer_owner_check_accepts_same_user_socket_pair() {
        let (client, _server) = UnixStream::pair().expect("socket pair");
        verify_peer_owner(&client, "test client").expect("same uid peer");
    }

    #[test]
    fn bind_socket_path_creates_private_parent_and_handles_collisions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_socket_path().with_extension("dir");
        let path = dir.join("mult.sock");
        bind_socket_path(&path).expect("create socket parent");
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(&dir).unwrap();

        let regular = unique_socket_path();
        fs::write(&regular, "keep").unwrap();
        assert_eq!(
            bind_socket_path(&regular).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&regular).unwrap(), "keep");
        fs::remove_file(&regular).unwrap();

        let stale = unique_socket_path();
        drop(UnixListener::bind(&stale).unwrap());
        bind_socket_path(&stale).expect("remove stale socket");
        assert!(!stale.exists());
    }

    #[test]
    fn handshake_rejects_wrong_version_and_non_hello_first_message() {
        for first in [
            ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION + 1,
                resume: None,
            },
            ClientMessage::ListSessions {
                namespace: test_identity().namespace,
            },
        ] {
            let (mut client_stream, server_stream) = UnixStream::pair().unwrap();
            client_stream
                .set_read_timeout(Some(TEST_IO_TIMEOUT))
                .unwrap();
            let server = Arc::new(Mutex::new(ServerState::default()));
            let (done_tx, done_rx) = mpsc::channel();
            let handle = thread::spawn(move || {
                let _ = done_tx.send(handle_client(server_stream, server));
            });
            write_message(&mut client_stream, &first).unwrap();
            assert!(matches!(
                read_message::<ServerMessage>(&mut client_stream).unwrap(),
                ServerMessage::Error { .. }
            ));
            done_rx
                .recv_timeout(TEST_IO_TIMEOUT)
                .expect("handler completion")
                .expect("handler result");
            handle.join().unwrap();
        }
    }

    #[test]
    fn dimensions_reservations_and_client_queue_remain_bounded() {
        let (rows, cols) = bounded_pty_dimensions(u16::MAX, u16::MAX);
        assert!(usize::from(rows) * usize::from(cols) <= mult_protocol::MAX_SCREEN_CELLS);
        assert!(rows > 0 && cols > 0);

        let mut server = ServerState::default();
        server
            .reserve_session(Some(SessionId(1)), test_identity())
            .unwrap();
        assert_eq!(server.allocate_session_id().unwrap(), SessionId(2));
        assert_eq!(
            server
                .reserve_session(Some(SessionId(1)), test_identity())
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        server.release_session_reservation(SessionId(1), test_identity());
        assert!(server.reserved_sessions.is_empty());

        let (sender, _receiver) = mpsc::sync_channel(1);
        let (stream, _peer) = UnixStream::pair().unwrap();
        let client = ClientHandle {
            id: 1,
            sender,
            stream: Arc::new(stream),
            active: Arc::new(AtomicBool::new(true)),
        };
        let sessions = || ServerMessage::Sessions {
            namespace: test_identity().namespace,
            sessions: Vec::new(),
        };
        assert!(client.try_deliver(sessions()));
        assert!(!client.try_deliver(sessions()));
    }

    #[test]
    fn shutdown_racing_reserved_create_never_publishes_the_child() {
        let mut server = ServerState::default();
        let session = server
            .reserve_session(Some(SessionId(77)), test_identity())
            .unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));

        // This is the publication boundary reached after an in-flight spawn.
        // Shutdown wins before the reserved child attempts to commit.
        server.shutting_down = true;
        let error = server
            .publish_reserved_session(session, pane)
            .expect_err("shutdown must reject reserved child publication");

        assert!(error.to_string().contains("shutting down"));
        assert!(server.sessions.is_empty());
        server.release_session_reservation(session, test_identity());
        assert!(server.reserved_sessions.is_empty());
        assert!(server.reserved_identities.is_empty());
    }

    #[test]
    fn shutdown_wins_at_the_attach_commit_boundary_and_caches_the_rejection() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let initial_lease = server.lock().unwrap().next_lease;
        let request_id = request_id(1);
        let request = attach_request(request_id, 5, 7);
        let (client, receiver) = test_client(1);
        let shutdown_server = Arc::clone(&server);

        handle_attach_request_with_hooks(
            &server,
            scope,
            false,
            &client,
            request.clone(),
            move || shutdown_server.lock().unwrap().shutting_down = true,
            || {},
        )
        .unwrap();
        let first = receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap();
        assert!(matches!(
            &first,
            ServerMessage::AttachResult {
                request_id: received,
                outcome: AttachOutcome::Error(AttachError::Failed { message }),
            } if *received == request_id && message == SHUTDOWN_ERROR_MESSAGE
        ));
        {
            let pane_state = pane.lock().unwrap();
            assert!(pane_state.owner.is_none());
            assert_eq!((pane_state.rows, pane_state.cols), (1, 1));
        }
        assert_eq!(server.lock().unwrap().next_lease, initial_lease);

        handle_attach_request(&server, scope, false, &client, request).unwrap();
        assert_eq!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            first,
            "a new request rejected by shutdown is cached"
        );
    }

    #[test]
    fn shutdown_wins_against_takeover_without_mutating_the_old_owner() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let old_scope = server.lock().unwrap().allocate_scope().unwrap();
        let new_scope = server.lock().unwrap().allocate_scope().unwrap();
        let (old_client, old_receiver) = test_client(1);
        let old_lease = AttachmentLease::MIN;
        let pane = Arc::new(Mutex::new(test_pane_state(Some(AttachmentOwner {
            scope: old_scope,
            lease: old_lease,
            client: old_client.clone(),
        }))));
        {
            let mut state = server.lock().unwrap();
            state.next_lease = old_lease.checked_next();
            state.sessions.insert(SessionId(1), Arc::clone(&pane));
        }
        let initial_next_lease = server.lock().unwrap().next_lease;
        let request_id = request_id(1);
        let (new_client, new_receiver) = test_client(2);
        let shutdown_server = Arc::clone(&server);

        handle_attach_request_with_hooks(
            &server,
            new_scope,
            false,
            &new_client,
            attach_request(request_id, 9, 11),
            move || shutdown_server.lock().unwrap().shutting_down = true,
            || {},
        )
        .unwrap();

        assert!(matches!(
            new_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::AttachResult {
                request_id: received,
                outcome: AttachOutcome::Error(AttachError::Failed { ref message }),
            } if received == request_id && message == SHUTDOWN_ERROR_MESSAGE
        ));
        let pane_state = pane.lock().unwrap();
        assert_eq!((pane_state.rows, pane_state.cols), (1, 1));
        assert!(pane_state.owner.as_ref().is_some_and(|owner| {
            owner.scope == old_scope && owner.lease == old_lease && owner.client.id == old_client.id
        }));
        drop(pane_state);
        assert_eq!(server.lock().unwrap().next_lease, initial_next_lease);
        assert!(old_receiver.try_recv().is_err(), "no TakenOver was emitted");
    }

    #[test]
    fn cached_success_cannot_rebind_after_shutdown_begins() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let request_id = request_id(1);
        let request = attach_request(request_id, 1, 1);
        let (first, first_receiver) = test_client(1);
        handle_attach_request(&server, scope, false, &first, request.clone()).unwrap();
        let lease = receive_empty_attach_transaction(&first_receiver, request_id);
        server.lock().unwrap().shutting_down = true;
        let (replacement, replacement_receiver) = test_client(2);

        handle_attach_request(&server, scope, true, &replacement, request).unwrap();

        assert!(matches!(
            replacement_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::AttachResult {
                request_id: received,
                outcome: AttachOutcome::Error(AttachError::Failed { ref message }),
            } if received == request_id && message == SHUTDOWN_ERROR_MESSAGE
        ));
        assert!(pane.lock().unwrap().owner.as_ref().is_some_and(|owner| {
            owner.scope == scope && owner.lease == lease && owner.client.id == first.id
        }));
        assert!(
            first_receiver.try_recv().is_err(),
            "no TakenOver was emitted"
        );
    }

    #[test]
    fn request_cache_replays_exact_results_and_rejects_collisions() {
        let mut server = ServerState::default();
        let scope = server.allocate_scope().expect("scope");
        let (client, _receiver) = test_client(1);
        let id = request_id(1);
        let request = create_request(id, "first");
        assert!(matches!(
            server.begin_request(scope, &request, &client),
            RequestDisposition::New
        ));
        let response = ServerMessage::CreateResult {
            request_id: id,
            outcome: CreateOutcome::Error(CreateError::Failed {
                message: "injected".to_string(),
            }),
        };
        server.complete_request(
            RequestKey {
                scope,
                request_id: id,
            },
            response.clone(),
        );
        assert!(matches!(
            server.begin_request(scope, &request, &client),
            RequestDisposition::Cached(cached) if cached == response
        ));
        assert!(matches!(
            server.begin_request(scope, &create_request(id, "different"), &client),
            RequestDisposition::Collision
        ));
        let mut different_identity = request.clone();
        let ClientMessage::CreateSession { identity, .. } = &mut different_identity else {
            unreachable!()
        };
        *identity = test_identity_bytes(0x41, 0x55);
        assert!(matches!(
            server.begin_request(scope, &different_identity, &client),
            RequestDisposition::Collision
        ));
    }

    #[test]
    fn agent_status_validates_schema_chat_generation_and_final_transitions() {
        let identity = test_identity();
        let metadata = test_agent_metadata();
        let mut server = ServerState::default();
        server.agent_states.insert(
            identity,
            DaemonAgentState {
                metadata,
                status: None,
            },
        );
        let record = |status| AgentStatusRecord {
            schema_version: AGENT_STATUS_SCHEMA_VERSION,
            identity,
            chat_id: metadata.chat_id,
            agent: metadata.agent,
            generation: metadata.generation,
            status,
        };

        let mut wrong_schema = record(AgentStatus::Running);
        wrong_schema.schema_version += 1;
        assert!(matches!(
            update_agent_status(&mut server, wrong_schema),
            AgentStatusOutcome::Error(AgentStatusError::WrongSchema { .. })
        ));

        let mut wrong_chat = record(AgentStatus::Running);
        wrong_chat.chat_id += 1;
        assert!(matches!(
            update_agent_status(&mut server, wrong_chat),
            AgentStatusOutcome::Error(AgentStatusError::WrongChat { .. })
        ));

        let mut stale = record(AgentStatus::Running);
        stale.generation = mult_protocol::AgentGeneration::from_bytes([0x66; 16]).unwrap();
        assert!(matches!(
            update_agent_status(&mut server, stale),
            AgentStatusOutcome::Error(AgentStatusError::StaleGeneration { .. })
        ));

        assert!(matches!(
            update_agent_status(&mut server, record(AgentStatus::Finished)),
            AgentStatusOutcome::Updated(AgentStatusRecord {
                status: AgentStatus::Finished,
                ..
            })
        ));
        assert!(matches!(
            update_agent_status(&mut server, record(AgentStatus::Running)),
            AgentStatusOutcome::Updated(AgentStatusRecord {
                status: AgentStatus::Running,
                ..
            })
        ));

        assert!(matches!(
            update_agent_status(&mut server, record(AgentStatus::Failed)),
            AgentStatusOutcome::Updated(AgentStatusRecord {
                status: AgentStatus::Failed,
                ..
            })
        ));
        assert!(matches!(
            update_agent_status(&mut server, record(AgentStatus::Running)),
            AgentStatusOutcome::Error(AgentStatusError::FinalStatusConflict {
                current: AgentStatus::Failed,
                attempted: AgentStatus::Running,
            })
        ));
        assert!(matches!(
            query_agent_status(
                &server,
                AgentStatusQuery {
                    schema_version: AGENT_STATUS_SCHEMA_VERSION,
                    identity,
                    chat_id: metadata.chat_id,
                    agent: metadata.agent,
                    generation: metadata.generation,
                }
            ),
            AgentStatusOutcome::Current(Some(AgentStatusRecord {
                status: AgentStatus::Failed,
                ..
            }))
        ));
    }

    #[test]
    fn concurrent_exact_request_waiters_receive_one_cached_completion() {
        let mut scope = RequestScopeState::new();
        let (first, _first_rx) = test_client(1);
        let (second, _second_rx) = test_client(2);
        let id = request_id(1);
        let request = create_request(id, "same");
        assert!(matches!(
            scope.begin(id, &request, &first),
            RequestDisposition::New
        ));
        assert!(matches!(
            scope.begin(id, &request, &second),
            RequestDisposition::Pending
        ));
        let response = ServerMessage::CreateResult {
            request_id: id,
            outcome: CreateOutcome::Error(CreateError::RetryExpired),
        };
        let waiters = scope.complete(id, response.clone());
        assert_eq!(
            waiters.iter().map(|waiter| waiter.id).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(matches!(
            scope.begin(id, &request, &second),
            RequestDisposition::Cached(cached) if cached == response
        ));
    }

    #[test]
    fn overload_rejection_consumes_and_caches_the_request_id() {
        let mut scope = RequestScopeState::new();
        let (client, _receiver) = test_client(1);
        for value in 1..=MAX_PENDING_REQUESTS_PER_CLIENT as u64 {
            let id = request_id(value);
            assert!(matches!(
                scope.begin(id, &create_request(id, "pending"), &client),
                RequestDisposition::New
            ));
        }
        let rejected_id = request_id(MAX_PENDING_REQUESTS_PER_CLIENT as u64 + 1);
        let rejected = create_request(rejected_id, "overloaded");
        assert!(matches!(
            scope.begin(rejected_id, &rejected, &client),
            RequestDisposition::TooManyPending
        ));
        let response = ServerMessage::CreateResult {
            request_id: rejected_id,
            outcome: CreateOutcome::Error(CreateError::Failed {
                message: "too many pending requests".to_string(),
            }),
        };
        scope.complete(rejected_id, response.clone());
        assert!(matches!(
            scope.begin(rejected_id, &rejected, &client),
            RequestDisposition::Cached(cached) if cached == response
        ));
    }

    #[test]
    fn evicted_request_ids_expire_instead_of_mutating_again() {
        let mut scope = RequestScopeState::new();
        let (client, _receiver) = test_client(1);
        for value in 1..=(MAX_CACHED_REQUEST_RESULTS_PER_SCOPE as u64 + 1) {
            let id = request_id(value);
            let request = create_request(id, "same");
            assert!(matches!(
                scope.begin(id, &request, &client),
                RequestDisposition::New
            ));
            scope.complete(
                id,
                ServerMessage::CreateResult {
                    request_id: id,
                    outcome: CreateOutcome::Error(CreateError::RetryExpired),
                },
            );
        }
        let first = create_request(request_id(1), "same");
        assert!(matches!(
            scope.begin(request_id(1), &first, &client),
            RequestDisposition::Expired
        ));
    }

    #[test]
    fn history_offsets_report_the_exact_retained_suffix() {
        let mut pane = test_pane_state(None);
        assert_eq!(
            pane.append_raw_history_with_limit(&shared(b"012345"), 10)
                .unwrap(),
            OutputSequence::ZERO
        );
        assert_eq!(
            pane.append_raw_history_with_limit(&shared(b"6789ABCD"), 10)
                .unwrap(),
            OutputSequence::new(6)
        );
        assert_eq!(pane.raw_history.to_vec(), b"456789ABCD");
        assert_eq!(pane.history_start, OutputSequence::new(4));
        assert_eq!(pane.next_output, OutputSequence::new(14));
    }

    #[test]
    fn termination_driver_orders_term_grace_kill_and_final_wait() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let signal_calls = Arc::clone(&calls);
        let wait_calls = Arc::clone(&calls);
        let mut waits = 0;

        let result = drive_termination(
            move |signal| {
                signal_calls
                    .lock()
                    .unwrap()
                    .push(if signal == libc::SIGTERM {
                        "term"
                    } else {
                        "kill"
                    });
                Ok(())
            },
            move |timeout| {
                waits += 1;
                wait_calls
                    .lock()
                    .unwrap()
                    .push(if timeout == STOP_TERM_GRACE {
                        "grace"
                    } else {
                        "final"
                    });
                waits == 2
            },
        );

        assert!(matches!(result, TerminationResult::Finalized));
        assert_eq!(*calls.lock().unwrap(), ["term", "grace", "kill", "final"]);
    }

    #[test]
    fn termination_failures_are_ordered_and_remain_recoverable() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let result = drive_termination(
            move |signal| {
                recorded.lock().unwrap().push(signal);
                if signal == libc::SIGTERM {
                    Ok(())
                } else {
                    Err(io::Error::other("injected KILL failure"))
                }
            },
            |_| false,
        );
        assert!(matches!(result, TerminationResult::KillFailed(_)));
        assert_eq!(*calls.lock().unwrap(), [libc::SIGTERM, libc::SIGKILL]);

        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        {
            let mut pane_state = pane.lock().unwrap();
            pane_state.lifecycle = PaneLifecycle::Stopping;
            pane_state.stop_driver_active = true;
        }
        record_wait_failure(&pane, "injected wait failure");
        let pane_state = pane.lock().unwrap();
        assert!(
            pane_state.stop_driver_active,
            "waiter cannot start a second driver"
        );
        assert_eq!(
            pane_state.last_wait_error.as_deref(),
            Some("injected wait failure")
        );
    }

    #[test]
    fn lease_exhaustion_completes_and_caches_attach_failure() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        server.lock().unwrap().next_lease = None;
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        server.lock().unwrap().sessions.insert(SessionId(1), pane);
        let (client, receiver) = test_client(1);
        let request = ClientMessage::Attach {
            request_id: request_id(1),
            identity: test_identity(),
            session: SessionId(1),
            rows: 1,
            cols: 1,
        };

        handle_attach_request(&server, scope, false, &client, request.clone()).unwrap();
        handle_attach_request(&server, scope, false, &client, request).unwrap();

        for _ in 0..2 {
            assert!(matches!(
                receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
                ServerMessage::AttachResult {
                    outcome: AttachOutcome::Error(AttachError::Failed { ref message }),
                    ..
                } if message.contains("lease space exhausted")
            ));
        }
    }

    #[test]
    fn failed_output_delivery_keeps_a_dormant_lease_for_exact_resumed_attach() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        // A queue with no writer thread: whatever a rejected mutation would
        // have written stays observable instead of vanishing into a PTY.
        let writes = PtyWriteQueue::with_capacity(PTY_WRITE_QUEUE_MAX_BYTES);
        pane.lock().unwrap().writes = Arc::clone(&writes);
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let request_id = request_id(1);
        let request = attach_request(request_id, 1, 1);
        let (first, first_receiver) = test_client_with_capacity(1, 8);

        handle_attach_request(&server, scope, false, &first, request.clone()).unwrap();
        let lease = receive_empty_attach_transaction(&first_receiver, request_id);
        fill_client_queue(&first, 8);

        publish_pty_output(&pane, shared(b"delivery-failed-but-retained")).unwrap();

        assert!(!first.is_active());
        {
            let pane_state = pane.lock().unwrap();
            assert!(pane_state.owner.as_ref().is_some_and(|owner| {
                owner.scope == scope && owner.lease == lease && owner.client.id == first.id
            }));
            assert_eq!(
                pane_state.validate_lease(scope, first.id, lease),
                Err(LeaseRejectionReason::NotOwner),
                "an inactive transport cannot mutate with its dormant lease"
            );
            assert_eq!(
                pane_state.validate_stop_lease(scope, first.id, lease),
                Err(LeaseRejectionReason::NotOwner)
            );
        }

        let (resumed_client, resumed_receiver) = test_client_with_capacity(2, 8);
        handle_attach_request(
            &server,
            scope,
            true,
            &resumed_client,
            attach_request(request_id, 1, 2),
        )
        .unwrap();
        assert!(matches!(
            resumed_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::AttachResult {
                request_id: received,
                outcome: AttachOutcome::Error(AttachError::RequestCollision),
            } if received == request_id
        ));
        handle_attach_request(&server, scope, false, &resumed_client, request.clone()).unwrap();
        assert!(matches!(
            resumed_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::AttachResult {
                request_id: received,
                outcome: AttachOutcome::Error(AttachError::Superseded),
            } if received == request_id
        ));
        assert_eq!(
            pane.lock().unwrap().owner.as_ref().unwrap().client.id,
            first.id
        );

        handle_attach_request(&server, scope, true, &resumed_client, request).unwrap();
        assert!(matches!(
            resumed_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::AttachResult {
                request_id: received,
                outcome: AttachOutcome::Attached { lease: received_lease, .. },
            } if received == request_id && received_lease == lease
        ));
        assert!(matches!(
            resumed_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayBegin {
                request_id: received,
                lease: received_lease,
                first_sequence: OutputSequence::ZERO,
                watermark,
                ..
            } if received == request_id
                && received_lease == lease
                && watermark == OutputSequence::new(28)
        ));
        assert!(matches!(
            resumed_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayChunk {
                request_id: received,
                lease: received_lease,
                sequence: OutputSequence::ZERO,
                ref bytes,
                ..
            } if received == request_id
                && received_lease == lease
                && bytes == b"delivery-failed-but-retained"
        ));
        assert!(matches!(
            resumed_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayEnd {
                request_id: received,
                lease: received_lease,
                watermark,
                ..
            } if received == request_id
                && received_lease == lease
                && watermark == OutputSequence::new(28)
        ));
        assert!(matches!(
            resumed_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ForegroundProcess { lease: received_lease, .. }
                if received_lease == lease
        ));
        let pane_state = pane.lock().unwrap();
        assert!(pane_state.owner.as_ref().is_some_and(|owner| {
            owner.scope == scope
                && owner.lease == lease
                && owner.client.id == resumed_client.id
                && owner.client.is_active()
        }));
        assert_eq!(
            pane_state.validate_lease(scope, first.id, lease),
            Err(LeaseRejectionReason::NotOwner)
        );
        assert_eq!(
            pane_state.validate_lease(scope, resumed_client.id, lease),
            Ok(())
        );
        drop(pane_state);

        handle_leased_input(
            &server,
            scope,
            &first,
            PaneId(1),
            lease,
            b"stale-input".to_vec(),
            LeaseOperation::Input,
        );
        handle_leased_input(
            &server,
            scope,
            &first,
            PaneId(1),
            lease,
            b"stale-paste".to_vec(),
            LeaseOperation::Paste,
        );
        handle_leased_resize(&server, scope, &first, PaneId(1), lease, 77, 99);
        handle_leased_detach(&server, scope, &first, PaneId(1), lease);
        let stale_stop_request_id = RequestId::new(2).unwrap();
        let stale_stop = ClientMessage::Stop {
            request_id: stale_stop_request_id,
            identity: test_identity(),
            pane: PaneId(1),
            lease,
        };
        handle_stop_request(&server, scope, &first, stale_stop.clone()).unwrap();
        handle_stop_request(&server, scope, &resumed_client, stale_stop).unwrap();
        assert!(matches!(
            resumed_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::StopResult {
                request_id: received,
                outcome: StopOutcome::Error(StopError::LeaseRejected(
                    LeaseRejectionReason::NotOwner
                )),
            } if received == stale_stop_request_id
        ));
        let pane_state = pane.lock().unwrap();
        assert_eq!(writes.queued_bytes(), 0, "no rejected byte reached the PTY");
        assert_eq!((pane_state.rows, pane_state.cols), (1, 1));
        assert_eq!(pane_state.lifecycle, PaneLifecycle::Running);
        assert!(pane_state.pending_stops.is_empty());
        assert!(pane_state.owner.as_ref().is_some_and(|owner| {
            owner.scope == scope
                && owner.lease == lease
                && owner.client.id == resumed_client.id
                && owner.client.is_active()
        }));
    }

    #[test]
    fn failed_foreground_delivery_keeps_a_dormant_lease_for_resumption() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let request_id = request_id(1);
        let request = attach_request(request_id, 1, 1);
        let (first, first_receiver) = test_client_with_capacity(1, 8);
        handle_attach_request(&server, scope, false, &first, request.clone()).unwrap();
        let lease = receive_empty_attach_transaction(&first_receiver, request_id);
        fill_client_queue(&first, 8);

        let process = pane.lock().unwrap().foreground_process.clone();
        deliver_foreground_process(&pane.lock().unwrap(), process);

        assert!(!first.is_active());
        assert!(pane.lock().unwrap().owner.as_ref().is_some_and(|owner| {
            owner.scope == scope && owner.lease == lease && owner.client.id == first.id
        }));

        let (resumed_client, resumed_receiver) = test_client_with_capacity(2, 8);
        handle_attach_request(&server, scope, true, &resumed_client, request).unwrap();
        assert_eq!(
            receive_empty_attach_transaction(&resumed_receiver, request_id),
            lease
        );
        assert_eq!(
            pane.lock().unwrap().owner.as_ref().unwrap().client.id,
            resumed_client.id
        );
    }

    #[test]
    fn failed_initial_attach_foreground_delivery_preserves_the_resumable_lease() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let request_id = request_id(1);
        let request = attach_request(request_id, 1, 1);
        let (first, first_receiver) = test_client_with_capacity(1, 3);

        handle_attach_request(&server, scope, false, &first, request.clone()).unwrap();

        let lease = match first_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap() {
            ServerMessage::AttachResult {
                request_id: received,
                outcome: AttachOutcome::Attached { lease, .. },
            } if received == request_id => lease,
            message => panic!("unexpected attach acknowledgement: {message:?}"),
        };
        assert!(matches!(
            first_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayBegin {
                request_id: received,
                lease: received_lease,
                ..
            } if received == request_id && received_lease == lease
        ));
        assert!(matches!(
            first_receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayEnd {
                request_id: received,
                lease: received_lease,
                ..
            } if received == request_id && received_lease == lease
        ));
        assert!(first_receiver.try_recv().is_err());
        assert!(
            first.is_active(),
            "an overflowed attach transaction leaves the attachment unreconciled; \
             disconnecting the connection it just attached produced a reconnect loop"
        );
        assert!(pane.lock().unwrap().owner.as_ref().is_some_and(|owner| {
            owner.scope == scope && owner.lease == lease && owner.client.id == first.id
        }));

        let (resumed, resumed_receiver) = test_client_with_capacity(2, 4);
        handle_attach_request(&server, scope, true, &resumed, request).unwrap();

        assert_eq!(
            receive_empty_attach_transaction(&resumed_receiver, request_id),
            lease
        );
        assert!(pane.lock().unwrap().owner.as_ref().is_some_and(|owner| {
            owner.scope == scope
                && owner.lease == lease
                && owner.client.id == resumed.id
                && owner.client.is_active()
        }));
    }

    #[test]
    fn fresh_takeover_of_a_dormant_owner_allocates_a_new_lease() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let old_scope = server.lock().unwrap().allocate_scope().unwrap();
        let new_scope = server.lock().unwrap().allocate_scope().unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let old_request_id = request_id(1);
        let old_request = attach_request(old_request_id, 1, 1);
        let (old, old_receiver) = test_client_with_capacity(1, 8);
        handle_attach_request(&server, old_scope, false, &old, old_request.clone()).unwrap();
        let old_lease = receive_empty_attach_transaction(&old_receiver, old_request_id);
        fill_client_queue(&old, 8);
        let process = pane.lock().unwrap().foreground_process.clone();
        deliver_foreground_process(&pane.lock().unwrap(), process);
        assert!(!old.is_active());

        let (new, new_receiver) = test_client_with_capacity(2, 8);
        handle_attach_request(
            &server,
            new_scope,
            false,
            &new,
            attach_request(request_id(1), 1, 1),
        )
        .unwrap();
        let new_lease = receive_empty_attach_transaction(&new_receiver, request_id(1));
        assert_ne!(new_lease, old_lease);
        assert!(pane.lock().unwrap().owner.as_ref().is_some_and(|owner| {
            owner.scope == new_scope && owner.lease == new_lease && owner.client.id == new.id
        }));

        let (old_resumed, old_resumed_receiver) = test_client(3);
        handle_attach_request(&server, old_scope, true, &old_resumed, old_request).unwrap();
        assert!(matches!(
            old_resumed_receiver
                .recv_timeout(TEST_IO_TIMEOUT)
                .unwrap(),
            ServerMessage::AttachResult {
                request_id: received,
                outcome: AttachOutcome::Error(AttachError::Superseded),
            } if received == old_request_id
        ));
    }

    #[test]
    fn numbered_output_contending_with_attach_replay_is_strictly_contiguous() {
        let scope = ClientScopeId::from_bytes([11; 16]);
        let request_id = request_id(7);
        let lease = AttachmentLease::MIN;
        let (client, receiver) = test_client_with_capacity(1, 128);
        let pane = Arc::new(Mutex::new(test_pane_state(Some(AttachmentOwner {
            scope,
            lease,
            client: client.clone(),
        }))));
        let record = |number: usize| format!("SEQ:{number:08}\n").into_bytes();
        let replay_bytes = (0..32).flat_map(&record).collect::<Vec<_>>();
        let expected_bytes = (0..64).flat_map(&record).collect::<Vec<_>>();
        pane.lock()
            .unwrap()
            .append_raw_history(&shared(&replay_bytes))
            .unwrap();
        let response = ServerMessage::AttachResult {
            request_id,
            outcome: AttachOutcome::Attached {
                session: SessionId(1),
                pane: pane.lock().unwrap().pane_info(),
                lease,
            },
        };
        let (start_sender, start_receiver) = mpsc::channel();
        let (contended_sender, contended_receiver) = mpsc::channel();
        let producer_pane = Arc::clone(&pane);
        let barrier_probe = Arc::clone(&pane);
        let producer = thread::spawn(move || {
            start_receiver.recv().unwrap();
            publish_pty_output_with_hook(&producer_pane, shared(&record(32)), || {
                assert!(matches!(
                    barrier_probe.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                contended_sender.send(()).unwrap();
            })
            .unwrap();
            for number in 33..64 {
                publish_pty_output(&producer_pane, shared(&record(number))).unwrap();
            }
        });

        let pane_state = pane.lock().unwrap();
        let replay_watermark = pane_state.next_output;
        let foreground = pane_state.foreground_process.clone();
        assert!(deliver_attach_transaction_with_hook(
            &client,
            &response,
            request_id,
            lease,
            &pane_state,
            &foreground,
            || {
                start_sender.send(()).unwrap();
                contended_receiver
                    .recv_timeout(TEST_IO_TIMEOUT)
                    .expect("producer reached the held pane replay barrier");
            },
        ));
        drop(pane_state);
        producer.join().unwrap();

        assert_eq!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            response,
            "the correlated acknowledgement is first"
        );
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayBegin {
                request_id: received,
                pane: PaneId(1),
                lease: received_lease,
                first_sequence: OutputSequence::ZERO,
                watermark,
                omitted_prefix_bytes: 0,
            } if received == request_id && received_lease == lease && watermark == replay_watermark
        ));
        let replay = match receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap() {
            ServerMessage::ReplayChunk {
                request_id: received,
                pane: PaneId(1),
                lease: received_lease,
                sequence: OutputSequence::ZERO,
                bytes,
            } if received == request_id && received_lease == lease => bytes,
            message => panic!("unexpected replay message: {message:?}"),
        };
        assert_eq!(replay, replay_bytes);
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayEnd {
                request_id: received,
                pane: PaneId(1),
                lease: received_lease,
                watermark,
            } if received == request_id && received_lease == lease && watermark == replay_watermark
        ));
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ForegroundProcess {
                pane: PaneId(1),
                lease: received_lease,
                ..
            } if received_lease == lease
        ));

        let mut combined = replay;
        let mut expected_sequence = replay_watermark;
        for number in 32..64 {
            match receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap() {
                ServerMessage::PtyOutput {
                    pane: PaneId(1),
                    lease: received_lease,
                    sequence,
                    bytes,
                } if received_lease == lease && sequence == expected_sequence => {
                    assert_eq!(bytes, record(number));
                    expected_sequence = expected_sequence.checked_add_bytes(bytes.len()).unwrap();
                    combined.extend_from_slice(&bytes);
                }
                message => panic!("live output overtook or broke replay ordering: {message:?}"),
            }
        }
        assert_eq!(
            combined, expected_bytes,
            "no numbered record is lost or duplicated"
        );
        assert_eq!(
            expected_sequence,
            pane.lock().unwrap().next_output,
            "live sequence reaches the exact publication watermark"
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn termination_driver_does_not_kill_after_graceful_completion() {
        let signals = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&signals);
        let result = drive_termination(
            move |signal| {
                recorded.lock().unwrap().push(signal);
                Ok(())
            },
            |timeout| timeout == STOP_TERM_GRACE,
        );

        assert!(matches!(result, TerminationResult::Finalized));
        assert_eq!(*signals.lock().unwrap(), [libc::SIGTERM]);
    }

    #[test]
    fn signal_process_group_rejects_the_daemon_group() {
        let own = unsafe { libc::getpgrp() };
        let error = signal_process_group(own, libc::SIGTERM).expect_err("reject own group");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn exit_info_preserves_child_status() {
        assert_eq!(exit_info(ExitStatus::with_exit_code(7)).code, 7);
        assert_eq!(
            exit_info(ExitStatus::with_signal("SIGTERM"))
                .signal
                .as_deref(),
            Some("SIGTERM")
        );
    }

    #[test]
    fn socket_permissions_are_user_only() {
        let path = unique_socket_path();
        let _listener = UnixListener::bind(&path).expect("bind socket");
        restrict_socket_permissions(&path).expect("restrict socket");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn command_line_formatting_is_shell_safe_for_display() {
        assert_eq!(
            command_line_from_cmdline_bytes(b"/usr/bin/cargo\0test\0space value\0"),
            Some("cargo test 'space value'".to_string())
        );
    }

    #[test]
    fn leases_are_connection_specific_and_stale_tokens_are_rejected() {
        let scope = ClientScopeId::from_bytes([7; 16]);
        let other_scope = ClientScopeId::from_bytes([8; 16]);
        let (owner, _owner_rx) = test_client(1);
        let lease = AttachmentLease::MIN;
        let pane = test_pane_state(Some(AttachmentOwner {
            scope,
            lease,
            client: owner.clone(),
        }));

        assert_eq!(pane.validate_lease(scope, 1, lease), Ok(()));
        assert_eq!(
            pane.validate_lease(scope, 2, lease),
            Err(LeaseRejectionReason::NotOwner)
        );
        assert_eq!(
            pane.validate_lease(other_scope, 1, lease),
            Err(LeaseRejectionReason::NotOwner)
        );
        assert_eq!(
            pane.validate_lease(scope, 1, lease.checked_next().expect("second lease")),
            Err(LeaseRejectionReason::StaleLease)
        );
        assert_eq!(pane.validate_stop_lease(scope, 1, lease), Ok(()));
        assert_eq!(
            pane.validate_stop_lease(scope, 2, lease),
            Err(LeaseRejectionReason::NotOwner)
        );
        owner.disconnect();
        assert_eq!(
            pane.validate_lease(scope, 1, lease),
            Err(LeaseRejectionReason::NotOwner)
        );
        assert_eq!(
            pane.validate_stop_lease(scope, 1, lease),
            Err(LeaseRejectionReason::NotOwner)
        );
    }

    #[test]
    fn stopping_and_shutdown_reject_all_non_stop_mutations() {
        let scope = ClientScopeId::from_bytes([9; 16]);
        let (owner, _receiver) = test_client(1);
        let lease = AttachmentLease::MIN;
        let mut pane = test_pane_state(Some(AttachmentOwner {
            scope,
            lease,
            client: owner,
        }));
        assert_eq!(pane.validate_mutation_lease(false, scope, 1, lease), Ok(()));
        pane.lifecycle = PaneLifecycle::Stopping;
        assert_eq!(
            pane.validate_mutation_lease(false, scope, 1, lease),
            Err(LeaseRejectionReason::NotOwner)
        );
        pane.lifecycle = PaneLifecycle::Running;
        assert_eq!(
            pane.validate_mutation_lease(true, scope, 1, lease),
            Err(LeaseRejectionReason::NotOwner)
        );
    }

    #[test]
    fn disconnect_keeps_logical_owner_for_scope_resumption() {
        let mut server = ServerState::default();
        let scope = server.allocate_scope().unwrap();
        let (owner, _receiver) = test_client(41);
        let pane = Arc::new(Mutex::new(test_pane_state(Some(AttachmentOwner {
            scope,
            lease: AttachmentLease::MIN,
            client: owner.clone(),
        }))));
        server.sessions.insert(SessionId(1), Arc::clone(&pane));

        owner.disconnect();
        server.remove_client(41);

        let pane = pane.lock().unwrap();
        assert!(pane
            .owner
            .as_ref()
            .is_some_and(|owner| { owner.scope == scope && owner.lease == AttachmentLease::MIN }));
        assert_eq!(
            pane.validate_lease(scope, 41, AttachmentLease::MIN),
            Err(LeaseRejectionReason::NotOwner)
        );
    }

    #[test]
    fn centralized_finalizer_handles_stop_before_natural_exit_exactly_once() {
        assert_finalization_order(true);
    }

    #[test]
    fn centralized_finalizer_handles_natural_exit_before_stop_exactly_once() {
        assert_finalization_order(false);
    }

    fn assert_finalization_order(stop_first: bool) {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let (client, receiver) = test_client(1);
        let lease = AttachmentLease::MIN;
        let request_id = request_id(1);
        let request = ClientMessage::Stop {
            request_id,
            identity: test_identity(),
            pane: PaneId(1),
            lease,
        };
        assert!(matches!(
            server
                .lock()
                .unwrap()
                .begin_request(scope, &request, &client),
            RequestDisposition::New
        ));
        let pane = Arc::new(Mutex::new(test_pane_state(Some(AttachmentOwner {
            scope,
            lease,
            client: client.clone(),
        }))));
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let exit = ExitInfo {
            code: 23,
            signal: None,
        };

        if stop_first {
            {
                let mut pane_state = pane.lock().unwrap();
                pane_state.lifecycle = PaneLifecycle::Stopping;
                pane_state
                    .pending_stops
                    .push(RequestKey { scope, request_id });
                pane_state.reader_done = true;
            }
            maybe_finalize(&server, &pane);
            assert!(server.lock().unwrap().sessions.contains_key(&SessionId(1)));
            assert!(receiver.try_recv().is_err());
            pane.lock().unwrap().child_exit = Some(exit.clone());
        } else {
            pane.lock().unwrap().child_exit = Some(exit.clone());
            maybe_finalize(&server, &pane);
            assert!(server.lock().unwrap().sessions.contains_key(&SessionId(1)));
            assert!(receiver.try_recv().is_err());
            {
                let mut pane_state = pane.lock().unwrap();
                pane_state.lifecycle = PaneLifecycle::Stopping;
                pane_state
                    .pending_stops
                    .push(RequestKey { scope, request_id });
            }
            maybe_finalize(&server, &pane);
            assert!(server.lock().unwrap().sessions.contains_key(&SessionId(1)));
            assert!(receiver.try_recv().is_err());
            pane.lock().unwrap().reader_done = true;
        }

        maybe_finalize(&server, &pane);
        maybe_finalize(&server, &pane);

        assert!(server.lock().unwrap().sessions.is_empty());
        assert_eq!(pane.lock().unwrap().lifecycle, PaneLifecycle::Removed);
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::PaneExited {
                pane: PaneId(1),
                lease: received_lease,
                exit: ref received_exit,
            } if received_lease == lease && received_exit == &exit
        ));
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::StopResult {
                request_id: received,
                outcome: StopOutcome::Stopped { exit: ref received_exit },
            } if received == request_id && received_exit == &exit
        ));
        assert!(
            receiver.try_recv().is_err(),
            "finalization was emitted twice"
        );
    }

    #[test]
    fn centralized_finalizer_removes_and_emits_each_result_once() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let (client, receiver) = test_client(1);
        let lease = AttachmentLease::MIN;
        let request_id = request_id(1);
        let request = ClientMessage::Stop {
            request_id,
            identity: test_identity(),
            pane: PaneId(1),
            lease,
        };
        assert!(matches!(
            server
                .lock()
                .unwrap()
                .begin_request(scope, &request, &client),
            RequestDisposition::New
        ));

        let mut pane_state = test_pane_state(Some(AttachmentOwner {
            scope,
            lease,
            client,
        }));
        pane_state.lifecycle = PaneLifecycle::Stopping;
        pane_state.reader_done = true;
        pane_state.child_exit = Some(ExitInfo {
            code: 0,
            signal: None,
        });
        pane_state
            .pending_stops
            .push(RequestKey { scope, request_id });
        let pane = Arc::new(Mutex::new(pane_state));
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));

        maybe_finalize(&server, &pane);
        maybe_finalize(&server, &pane);

        assert!(server.lock().unwrap().sessions.is_empty());
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::PaneExited {
                pane: PaneId(1),
                ..
            }
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            ServerMessage::StopResult {
                request_id: received,
                outcome: StopOutcome::Stopped { .. },
            } if received == request_id
        ));
        assert!(
            receiver.try_recv().is_err(),
            "finalizer emitted a duplicate"
        );
    }

    fn attached_pane(
        server: &SharedServer,
        scope: ClientScopeId,
        client: &ClientHandle,
        lease: AttachmentLease,
    ) -> SharedPane {
        let pane = Arc::new(Mutex::new(test_pane_state(Some(AttachmentOwner {
            scope,
            lease,
            client: client.clone(),
        }))));
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        pane
    }

    /// A2: a child that stops reading its stdin used to freeze the daemon,
    /// because `write_all` + `flush` ran on the socket-reader thread with both
    /// the server and pane mutex held.
    #[test]
    fn a_wedged_pty_write_holds_neither_the_server_nor_the_pane_lock() {
        struct BlockingWriter {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }

        impl Write for BlockingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let _ = self.entered.send(());
                let _ = self.release.recv();
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let lease = AttachmentLease::MIN;
        let (client, receiver) = test_client(1);
        let pane = attached_pane(&server, scope, &client, lease);
        pane.lock().unwrap().writes = PtyWriteQueue::spawn(Box::new(BlockingWriter {
            entered: entered_tx,
            release: release_rx,
        }));

        // Run the handler off-thread so a regression fails this deadline rather
        // than hanging the suite.
        let (done_tx, done_rx) = mpsc::channel();
        let input_server = Arc::clone(&server);
        let input_client = client.clone();
        thread::spawn(move || {
            handle_leased_input(
                &input_server,
                scope,
                &input_client,
                PaneId(1),
                lease,
                b"wedge".to_vec(),
                LeaseOperation::Input,
            );
            let _ = done_tx.send(());
        });

        done_rx
            .recv_timeout(TEST_IO_TIMEOUT)
            .expect("input returned without waiting for the PTY write");
        entered_rx
            .recv_timeout(TEST_IO_TIMEOUT)
            .expect("the writer thread reached the blocking write");

        // The PTY write is in flight and parked. The daemon must still be
        // completely usable.
        assert!(
            server.try_lock().is_ok(),
            "a wedged PTY write held the global server lock"
        );
        assert!(
            pane.try_lock().is_ok(),
            "a wedged PTY write held the pane lock"
        );
        assert!(
            receiver.try_recv().is_err(),
            "an accepted write is accepted"
        );
        let _ = release_tx.send(());
    }

    /// A2: the bounded queue refuses rather than dropping or blocking.
    #[test]
    fn a_full_pty_write_queue_refuses_instead_of_dropping_bytes() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let lease = AttachmentLease::MIN;
        let (client, receiver) = test_client(1);
        let pane = attached_pane(&server, scope, &client, lease);
        // No writer thread: exactly a child that never reads its stdin.
        let writes = PtyWriteQueue::with_capacity(8);
        pane.lock().unwrap().writes = Arc::clone(&writes);

        handle_leased_input(
            &server,
            scope,
            &client,
            PaneId(1),
            lease,
            b"12345678".to_vec(),
            LeaseOperation::Input,
        );
        assert_eq!(writes.queued_bytes(), 8);
        assert!(receiver.try_recv().is_err());

        handle_leased_input(
            &server,
            scope,
            &client,
            PaneId(1),
            lease,
            b"9".to_vec(),
            LeaseOperation::Paste,
        );

        assert_eq!(
            writes.queued_bytes(),
            8,
            "a refused write is never partially accepted"
        );
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::LeaseRejected {
                pane: PaneId(1),
                lease: rejected,
                operation: LeaseOperation::Paste,
                ..
            } if rejected == lease
        ));
        assert!(
            client.is_active(),
            "a pane-scoped refusal never closes the connection"
        );
    }

    #[test]
    fn queued_pty_input_reaches_the_master_through_the_writer_thread() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let lease = AttachmentLease::MIN;
        let (client, _receiver) = test_client(1);
        let pane = attached_pane(&server, scope, &client, lease);
        pane.lock().unwrap().writes =
            PtyWriteQueue::spawn(Box::new(RecordingWriter(Arc::clone(&written))));

        handle_leased_input(
            &server,
            scope,
            &client,
            PaneId(1),
            lease,
            b"hello".to_vec(),
            LeaseOperation::Input,
        );

        let deadline = Instant::now() + TEST_IO_TIMEOUT;
        while written.lock().unwrap().len() < 5 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*written.lock().unwrap(), b"hello");
    }

    /// N1: an attach must not serialize the whole daemon, but must still hold
    /// the pane barrier that orders replay against live output.
    #[test]
    fn attach_replay_releases_the_server_lock_and_keeps_the_pane_barrier() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        pane.lock()
            .unwrap()
            .append_raw_history(&shared(b"scrollback"))
            .unwrap();
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let request_id = request_id(1);
        let (client, receiver) = test_client_with_capacity(1, 16);
        let probe_server = Arc::clone(&server);
        let probe_pane = Arc::clone(&pane);

        handle_attach_request_with_hooks(
            &server,
            scope,
            false,
            &client,
            attach_request(request_id, 1, 1),
            || {},
            move || {
                assert!(
                    probe_server.try_lock().is_ok(),
                    "attach replay still held the global server lock"
                );
                assert!(
                    matches!(
                        probe_pane.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ),
                    "attach replay must keep the pane barrier (docs/DAEMON.md, replay ordering)"
                );
            },
        )
        .unwrap();

        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::AttachResult {
                outcome: AttachOutcome::Attached { .. },
                ..
            }
        ));
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayBegin { .. }
        ));
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayChunk { ref bytes, .. } if bytes == b"scrollback"
        ));
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayEnd { .. }
        ));
    }

    /// N1: an overflowed replay must leave the connection alive.
    #[test]
    fn replay_overflow_leaves_the_freshly_attached_client_connected() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        for _ in 0..4 {
            pane.lock()
                .unwrap()
                .append_raw_history(&shared(b"history"))
                .unwrap();
        }
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let request_id = request_id(1);
        // Two slots cannot hold the acknowledgement, ReplayBegin, four chunks,
        // ReplayEnd and the foreground frame.
        let (client, receiver) = test_client_with_capacity(1, 2);

        handle_attach_request(
            &server,
            scope,
            false,
            &client,
            attach_request(request_id, 1, 1),
        )
        .unwrap();

        assert!(
            client.is_active(),
            "the client that just attached must not be disconnected by its own replay"
        );
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::AttachResult {
                outcome: AttachOutcome::Attached { .. },
                ..
            }
        ));
        assert!(matches!(
            receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap(),
            ServerMessage::ReplayBegin { .. }
        ));
        assert!(
            pane.lock().unwrap().owner.is_some(),
            "the lease is retained so a fresh attach can reconcile"
        );
    }

    /// N3: a queued replay must not make the pane's history resident twice.
    #[test]
    fn queued_replay_chunks_share_the_pane_history_instead_of_copying_it() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        for value in 0..3_u8 {
            pane.lock()
                .unwrap()
                .append_raw_history(&shared(&[value; 16]))
                .unwrap();
        }
        let history = pane
            .lock()
            .unwrap()
            .raw_history
            .replay_chunks()
            .iter()
            .map(|chunk| chunk.as_ptr() as usize)
            .collect::<Vec<_>>();
        assert_eq!(history.len(), 3);
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        let request_id = request_id(1);
        let (client, receiver) = test_client_with_capacity(1, 16);

        handle_attach_request(
            &server,
            scope,
            false,
            &client,
            attach_request(request_id, 1, 1),
        )
        .unwrap();

        let mut queued = Vec::new();
        for _ in 0..7 {
            if let ClientDelivery::Replay { bytes, .. } = receiver
                .recv_delivery_timeout(TEST_IO_TIMEOUT)
                .expect("attach transaction delivery")
            {
                queued.push(bytes.as_ptr() as usize);
            }
        }
        assert_eq!(
            queued, history,
            "queued replay chunks must reference the pane's own retained chunks"
        );
    }

    /// A9: a live frame shares the allocation the history retains.
    #[test]
    fn live_output_frames_share_the_retained_history_allocation() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let scope = server.lock().unwrap().allocate_scope().unwrap();
        let lease = AttachmentLease::MIN;
        let (client, receiver) = test_client(1);
        let pane = attached_pane(&server, scope, &client, lease);
        let bytes = shared(b"once");

        publish_pty_output(&pane, Arc::clone(&bytes)).unwrap();

        match receiver
            .recv_delivery_timeout(TEST_IO_TIMEOUT)
            .expect("live output delivery")
        {
            ClientDelivery::Output { bytes: queued, .. } => assert_eq!(
                queued.as_ptr(),
                bytes.as_ptr(),
                "a queued frame must not copy what the pane already retains"
            ),
            other => panic!("unexpected delivery: {other:?}"),
        }
        assert_eq!(
            pane.lock().unwrap().raw_history.chunks[0].as_ptr(),
            bytes.as_ptr()
        );
    }

    /// N2: shutdown is bounded, and the socket is unlinked on every exit path.
    #[test]
    fn shutdown_drain_is_bounded_and_always_unlinks_the_socket() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        assert!(wait_for_sessions_drained(
            &server,
            Duration::from_millis(500)
        ));

        // A pane whose stop driver reported TimedOut is never removed. The old
        // unbounded spin then kept the daemon — and its socket — alive forever.
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::new(Mutex::new(test_pane_state(None))));
        let started = Instant::now();
        assert!(!wait_for_sessions_drained(
            &server,
            Duration::from_millis(100)
        ));
        assert!(
            started.elapsed() < TEST_IO_TIMEOUT,
            "the shutdown wait must be bounded"
        );

        let path = unique_socket_path();
        let listener = UnixListener::bind(&path).expect("bind socket");
        let guard = SocketGuard::new(path.clone());
        drop(listener);
        assert!(path.exists());
        drop(guard);
        assert!(!path.exists(), "every exit path unlinks the socket");
    }

    /// A1: trimming releases whole chunks and never moves a retained byte.
    #[test]
    fn history_trimming_drops_whole_chunks_without_moving_retained_bytes() {
        let mut pane = test_pane_state(None);
        let limit = 44;
        let chunk = |value: u8| shared(&[value; 8]);
        for value in 0..6 {
            pane.append_raw_history_with_limit(&chunk(value), limit)
                .unwrap();
        }
        assert_eq!(pane.raw_history.len(), 44);
        assert_eq!(pane.history_start, OutputSequence::new(4));
        let retained = pane
            .raw_history
            .chunks
            .iter()
            .map(|chunk| chunk.as_ptr() as usize)
            .collect::<Vec<_>>();
        assert_eq!(retained.len(), 6);

        for value in 6..8 {
            pane.append_raw_history_with_limit(&chunk(value), limit)
                .unwrap();
        }

        let surviving = pane
            .raw_history
            .chunks
            .iter()
            .map(|chunk| chunk.as_ptr() as usize)
            .collect::<Vec<_>>();
        // Two whole chunks were released and an offset advanced into the third.
        // Every surviving chunk is the *same allocation* it was before, so no
        // retained byte was memmoved; the old flat `Vec` copied all 44 twice.
        assert_eq!(surviving.len(), 6);
        assert_eq!(
            surviving[..4],
            retained[2..],
            "a trim copied bytes it was supposed to retain"
        );
        assert_eq!(pane.raw_history.len(), 44);
        assert_eq!(pane.history_start, OutputSequence::new(20));
        assert_eq!(pane.next_output, OutputSequence::new(64));
        let bytes = pane.raw_history.to_vec();
        assert_eq!(bytes.len(), 44);
        assert_eq!(bytes[0], 2, "the retained suffix starts mid-chunk");
        assert_eq!(bytes[43], 7);
    }

    /// A12: the cap is a scrollback budget, not the wire frame limit.
    #[test]
    fn retained_history_is_sized_from_the_client_scrollback() {
        assert_eq!(
            RAW_HISTORY_MAX_BYTES,
            RAW_HISTORY_SCROLLBACK_LINES * RAW_HISTORY_BYTES_PER_LINE
        );
        const { assert!(RAW_HISTORY_MAX_BYTES < MAX_MESSAGE_BYTES / 4) };

        let mut pane = test_pane_state(None);
        let read = shared(&[b'x'; 8192]);
        for _ in 0..(RAW_HISTORY_MAX_BYTES / 8192 + 8) {
            pane.append_raw_history(&read).unwrap();
        }
        assert!(pane.raw_history.len() <= RAW_HISTORY_MAX_BYTES);
        assert!(pane.raw_history.len() > RAW_HISTORY_MAX_BYTES - 8192);
    }

    /// A8: the reader path shares the input path's debounced poll instead of
    /// probing `tcgetpgrp` and `/proc/<pid>/cmdline` after every 8 KiB read.
    #[test]
    fn pty_reads_debounce_the_foreground_poll_instead_of_probing_every_read() {
        let scope = ClientScopeId::from_bytes([13; 16]);
        let lease = AttachmentLease::MIN;
        let (client, receiver) = test_client(1);
        let pane = Arc::new(Mutex::new(test_pane_state(Some(AttachmentOwner {
            scope,
            lease,
            client,
        }))));
        let scheduled = {
            let pane_state = pane.lock().unwrap();
            Arc::clone(&pane_state.foreground_poll_scheduled)
        };

        assert!(handle_pty_read(&pane, &scheduled, shared(b"first")).unwrap());
        assert!(
            !handle_pty_read(&pane, &scheduled, shared(b"second")).unwrap(),
            "a second read reuses the poll the first one scheduled"
        );

        for expected in [&b"first"[..], &b"second"[..]] {
            match receiver.recv_timeout(TEST_IO_TIMEOUT).unwrap() {
                ServerMessage::PtyOutput { bytes, .. } => assert_eq!(bytes, expected),
                message => panic!("unexpected delivery: {message:?}"),
            }
        }
        assert!(
            receiver.try_recv().is_err(),
            "the reader path must not broadcast a foreground process per read"
        );
    }

    /// A11: routing is a map lookup, never a scan that locks every pane while
    /// the server lock is held.
    #[test]
    fn pane_lookup_is_a_map_hit_and_never_locks_another_pane() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let pane = Arc::new(Mutex::new(test_pane_state(None)));
        server
            .lock()
            .unwrap()
            .sessions
            .insert(SessionId(1), Arc::clone(&pane));
        assert!(server.lock().unwrap().pane_by_id(PaneId(1)).is_some());
        {
            let pane_state = pane.lock().unwrap();
            assert_eq!(
                pane_state.pane.0, pane_state.session.0,
                "PaneId and SessionId are one daemon coordinate"
            );
        }

        let held = pane.lock().unwrap();
        let (found_tx, found_rx) = mpsc::channel();
        let probe = Arc::clone(&server);
        thread::spawn(move || {
            let _ = found_tx.send(probe.lock().unwrap().pane_by_id(PaneId(999)).is_some());
        });

        assert_eq!(
            found_rx.recv_timeout(TEST_IO_TIMEOUT),
            Ok(false),
            "a routing miss must not scan and lock every pane under the server lock"
        );
        drop(held);
    }

    fn test_pane_state(owner: Option<AttachmentOwner>) -> PaneState {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 1,
                cols: 1,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open test PTY");
        PaneState {
            session: SessionId(1),
            identity: test_identity(),
            agent: None,
            pane: PaneId(1),
            name: "test".to_string(),
            title: "test".to_string(),
            rows: 1,
            cols: 1,
            raw_history: RawHistory::default(),
            history_start: OutputSequence::ZERO,
            next_output: OutputSequence::ZERO,
            master: Arc::new(Mutex::new(pair.master)),
            // No writer thread by default: tests that care assert on the queue,
            // and the rest must not leak a thread per pane.
            writes: PtyWriteQueue::with_capacity(PTY_WRITE_QUEUE_MAX_BYTES),
            child_pid: std::process::id(),
            process_group: unsafe { libc::getpgrp() },
            foreground_process: ForegroundProcessInfo {
                root_pid: Some(std::process::id()),
                foreground_pid: None,
                command: None,
            },
            owner,
            lifecycle: PaneLifecycle::Running,
            reader_done: false,
            child_exit: None,
            last_wait_error: None,
            stop_driver_active: false,
            pending_stops: Vec::new(),
            lifecycle_signal: Arc::new((Mutex::new(0), Condvar::new())),
            foreground_poll_scheduled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn unique_socket_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        env::temp_dir().join(format!("mult-server-test-{unique}.sock"))
    }
}
