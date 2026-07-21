use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env, fs,
    io::{self, Write},
    os::unix::{io::AsRawFd, net::UnixStream, process::CommandExt},
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
    default_socket_path, read_message, write_message, AgentSessionMetadata, AgentStatusError,
    AgentStatusOutcome, AgentStatusQuery, AgentStatusRecord, AttachError, AttachOutcome,
    AttachmentLease, ClientMessage, ClientScopeId, CreateError, CreateOutcome,
    ForegroundProcessInfo, LaunchSpec, LeaseRejectionReason, OutputSequence, PaneId, RequestId,
    ServerInstanceId, ServerMessage, SessionId, SessionIdentity as WireSessionIdentity,
    SessionInfo, StateNamespace as WireStateNamespace, StopError, StopOutcome,
    MAX_PENDING_REQUESTS_PER_CLIENT, PROTOCOL_VERSION, SOCKET_PATH_ENV,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    Scrollback {
        terminal: PtyKey,
        bytes: Vec<u8>,
    },
    ReplayTruncated {
        terminal: PtyKey,
        omitted_bytes: u64,
    },
    Output {
        terminal: PtyKey,
        bytes: Vec<u8>,
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
    terminal_to_pane: HashMap<PtyKey, PaneId>,
    pane_to_terminal: HashMap<PaneId, PtyKey>,
    pane_leases: HashMap<PaneId, AttachmentLease>,
    expected_output: HashMap<PaneId, OutputSequence>,
    session_identities: HashMap<PtyKey, WireSessionIdentity>,
    agent_sessions: HashMap<PtyKey, AgentSessionMetadata>,
    parsers: HashMap<PtyKey, Parser>,
    responders: HashMap<PtyKey, TerminalResponseDetector>,
    terminals_with_output: HashSet<PtyKey>,
    terminal_exit_statuses: HashMap<PtyKey, PtyExit>,
    foreground_processes: HashMap<PtyKey, ForegroundProcessInfo>,
    command_trackers: HashMap<PtyKey, TerminalCommandTracker>,
    pending_events: Vec<PtyEvent>,
    client_scope: Option<ClientScopeId>,
    server_instance: Option<ServerInstanceId>,
    next_request_id: Option<RequestId>,
    pending_requests: HashSet<RequestId>,
    deferred_messages: VecDeque<ServerMessage>,
    // The terminal currently being created by `start`, if any. It is excluded
    // from reconnect re-attach because its session does not exist on the server
    // until `start`'s own CreateSession completes.
    starting: Option<PtyKey>,
}

const SERVER_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const ATTACH_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_EVENT_QUEUE_CAPACITY: usize = 4_096;
const TERMINAL_SCROLLBACK_LINES: usize = 5_000;
const TERMINAL_MAX_CSI_SEQUENCE_BYTES: usize = 128;
const PRIMARY_DEVICE_ATTRIBUTES_RESPONSE: &[u8] = b"\x1b[?1;2c";
const DEVICE_STATUS_OK_RESPONSE: &[u8] = b"\x1b[0n";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
/// xterm button bytes reported for the scroll wheel (bit 6 set marks a wheel
/// event; the low bit distinguishes up from down).
const WHEEL_UP_BUTTON: u8 = 64;
const WHEEL_DOWN_BUTTON: u8 = 65;

struct ServerConnection {
    writer: Arc<Mutex<UnixStream>>,
    receiver: Receiver<ServerMessage>,
}

#[derive(Debug, Default)]
struct TerminalResponseDetector {
    state: TerminalResponseState,
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
    Csi(Vec<u8>),
    CsiIgnored,
    String {
        esc_seen: bool,
    },
    IgnoreOne,
}

impl Default for PtyRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyRuntime {
    pub fn new() -> Self {
        Self::with_socket_path(default_socket_path())
    }

    pub fn new_offline() -> Self {
        Self::disconnected(default_socket_path(), Vec::new())
    }

    pub fn with_socket_path(socket_path: PathBuf) -> Self {
        match Self::connect_to_socket(socket_path.clone()) {
            Ok(runtime) => runtime,
            Err(error) => Self::disconnected(
                socket_path,
                vec![PtyEvent::Error {
                    terminal: PtyKey::Terminal(TerminalId(0)),
                    message: format!("failed to connect to mult-server: {error}"),
                }],
            ),
        }
    }

    pub fn connect_to_socket(socket_path: PathBuf) -> io::Result<Self> {
        let mut runtime = Self::disconnected(socket_path, Vec::new());
        runtime.connect()?;
        Ok(runtime)
    }

    fn disconnected(socket_path: PathBuf, pending_events: Vec<PtyEvent>) -> Self {
        Self {
            socket_path,
            connection: None,
            terminal_to_pane: HashMap::new(),
            pane_to_terminal: HashMap::new(),
            pane_leases: HashMap::new(),
            expected_output: HashMap::new(),
            session_identities: HashMap::new(),
            agent_sessions: HashMap::new(),
            parsers: HashMap::new(),
            responders: HashMap::new(),
            terminals_with_output: HashSet::new(),
            terminal_exit_statuses: HashMap::new(),
            foreground_processes: HashMap::new(),
            command_trackers: HashMap::new(),
            pending_events,
            client_scope: None,
            server_instance: None,
            next_request_id: Some(RequestId::MIN),
            pending_requests: HashSet::new(),
            deferred_messages: VecDeque::new(),
            starting: None,
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

impl PtyRuntime {
    pub fn is_running(&self, terminal: PtyKey) -> bool {
        self.terminal_to_pane.get(&terminal).is_some_and(|pane| {
            self.pane_leases.contains_key(pane) && self.expected_output.contains_key(pane)
        })
    }

    /// Bind a durable model identity to its runtime key. Rebinding a key to a
    /// different token is refused, so create/attach/stop always use one
    /// immutable logical identity for the lifetime of this adapter.
    pub fn register_session_identity(
        &mut self,
        terminal: PtyKey,
        identity: SessionIdentity,
    ) -> io::Result<()> {
        let identity = wire_session_identity(identity);
        if self
            .session_identities
            .get(&terminal)
            .is_some_and(|current| *current != identity)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot replace an immutable PTY session identity",
            ));
        }
        self.session_identities.insert(terminal, identity);
        Ok(())
    }

    pub fn registered_session_identity(&self, terminal: PtyKey) -> Option<WireSessionIdentity> {
        self.session_identities.get(&terminal).copied()
    }

    pub fn register_agent_session(
        &mut self,
        terminal: PtyKey,
        metadata: AgentSessionMetadata,
    ) -> io::Result<()> {
        if !matches!(terminal, PtyKey::ChatAgent(_)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "agent metadata may only be registered for a chat PTY",
            ));
        }
        if !self.session_identities.contains_key(&terminal) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "register the durable session identity before agent metadata",
            ));
        }
        if self.is_running(terminal)
            && self
                .agent_sessions
                .get(&terminal)
                .is_some_and(|current| *current != metadata)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cannot replace agent generation while its PTY is running",
            ));
        }
        self.agent_sessions.insert(terminal, metadata);
        Ok(())
    }

    pub fn parser(&self, terminal: PtyKey) -> Option<&Parser> {
        self.parsers.get(&terminal)
    }

    pub fn terminal_exit_status(&self, terminal: PtyKey) -> Option<&PtyExit> {
        self.terminal_exit_statuses.get(&terminal)
    }

    pub fn terminal_last_command(&self, terminal: PtyKey) -> Option<&str> {
        self.command_trackers
            .get(&terminal)
            .and_then(TerminalCommandTracker::last_command)
    }

    #[cfg(test)]
    pub fn mark_running_for_test(&mut self, terminal: PtyKey) {
        let pane = pane_for_key(terminal);
        self.terminal_to_pane.insert(terminal, pane);
        self.pane_to_terminal.insert(pane, terminal);
        self.pane_leases.insert(pane, AttachmentLease::MIN);
        self.expected_output.insert(pane, OutputSequence::ZERO);
    }

    #[cfg(test)]
    pub fn record_exit_status_for_test(&mut self, terminal: PtyKey, status: PtyExit) {
        self.terminal_exit_statuses.insert(terminal, status);
    }

    pub fn ensure_parser(&mut self, terminal: PtyKey, size: PtyDimensions) {
        self.parsers.entry(terminal).or_insert_with(|| {
            Parser::new(
                size.rows.max(1),
                size.cols.max(1),
                TERMINAL_SCROLLBACK_LINES,
            )
        });
        self.resize_parser(terminal, size);
    }

    pub fn reset_parser(&mut self, terminal: PtyKey, size: PtyDimensions) {
        self.parsers.insert(
            terminal,
            Parser::new(
                size.rows.max(1),
                size.cols.max(1),
                TERMINAL_SCROLLBACK_LINES,
            ),
        );
        self.responders.remove(&terminal);
        self.terminals_with_output.remove(&terminal);
        self.terminal_exit_statuses.remove(&terminal);
    }

    pub fn remove_terminal(&mut self, terminal: PtyKey) {
        if let Some(pane) = self.terminal_to_pane.remove(&terminal) {
            self.pane_to_terminal.remove(&pane);
            self.pane_leases.remove(&pane);
            self.expected_output.remove(&pane);
        }
        self.parsers.remove(&terminal);
        self.responders.remove(&terminal);
        self.terminals_with_output.remove(&terminal);
        self.terminal_exit_statuses.remove(&terminal);
        self.foreground_processes.remove(&terminal);
        self.command_trackers.remove(&terminal);
        self.session_identities.remove(&terminal);
        self.agent_sessions.remove(&terminal);
    }

    pub fn process_terminal_output(&mut self, terminal: PtyKey, bytes: &[u8]) {
        self.feed_terminal_output(terminal, bytes, false);
    }

    fn feed_terminal_output(&mut self, terminal: PtyKey, bytes: &[u8], respond: bool) {
        if bytes.is_empty() {
            return;
        }

        let responses = {
            let parser = self
                .parsers
                .entry(terminal)
                .or_insert_with(|| Parser::new(24, 80, TERMINAL_SCROLLBACK_LINES));
            let responses = if respond {
                let responder = self.responders.entry(terminal).or_default();
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
            responses
        };

        self.terminals_with_output.insert(terminal);
        if let Some(pane) = self.terminal_to_pane.get(&terminal).copied() {
            for response in responses {
                let _ = self.send_input_inner(terminal, pane, &response, false);
            }
        }
    }

    pub fn append_terminal_system_line(&mut self, terminal: PtyKey, message: impl AsRef<str>) {
        let line = format!("[mult] {}\r\n", message.as_ref());
        self.process_terminal_output(terminal, line.as_bytes());
    }

    pub fn terminal_lines(&self, terminal: PtyKey) -> Vec<String> {
        let Some(parser) = self.parsers.get(&terminal) else {
            return Vec::new();
        };
        terminal_screen_rows(parser)
    }

    pub fn terminal_all_lines(&self, terminal: PtyKey) -> Vec<String> {
        self.terminal_lines(terminal)
    }

    pub fn terminal_output_is_blank(&self, terminal: PtyKey) -> bool {
        if self.terminals_with_output.contains(&terminal) {
            return false;
        }
        self.parsers
            .get(&terminal)
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
    ) -> io::Result<AttachExistingResult> {
        if self.is_running(terminal) {
            return Ok(AttachExistingResult::Attached);
        }
        self.ensure_connected()?;
        let identity = self.identity_for_key(terminal)?;
        if self.is_running(terminal) {
            return Ok(AttachExistingResult::Attached);
        }
        self.reset_parser(terminal, size);
        self.foreground_processes.remove(&terminal);
        self.command_trackers.remove(&terminal);
        let session = session_for_key(terminal);
        let pane = pane_for_key(terminal);
        self.terminal_to_pane.insert(terminal, pane);
        self.pane_to_terminal.insert(pane, terminal);

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
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.clear_attachment(pane);
                Ok(AttachExistingResult::Missing)
            }
            Err(error) => {
                if !is_reconciliation_uncertain(&error) {
                    self.clear_attachment(pane);
                }
                Err(error)
            }
        }
    }

    pub fn start(&mut self, spawn: PtySpawn) -> io::Result<()> {
        if self.is_running(spawn.terminal) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal already has a server attachment",
            ));
        }
        if self.terminal_to_pane.contains_key(&spawn.terminal) {
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

    fn start_attached(&mut self, spawn: PtySpawn) -> io::Result<()> {
        self.ensure_connected()?;
        self.reset_parser(spawn.terminal, spawn.size);
        self.foreground_processes.remove(&spawn.terminal);
        self.command_trackers.remove(&spawn.terminal);
        let identity = self.identity_for_key(spawn.terminal)?;
        let session = session_for_key(spawn.terminal);
        let pane = pane_for_key(spawn.terminal);
        let launch = launch_spec(&spawn);
        let name = session_name(&spawn, &launch);
        self.terminal_to_pane.insert(spawn.terminal, pane);
        self.pane_to_terminal.insert(pane, spawn.terminal);

        let create_id = self.allocate_request()?;
        let create = ClientMessage::CreateSession {
            request_id: create_id,
            identity,
            requested_id: Some(session),
            agent: self.agent_sessions.get(&spawn.terminal).copied(),
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
            if !is_reconciliation_uncertain(&error) {
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
            if !is_reconciliation_uncertain(error) {
                self.clear_attachment(pane);
            }
        }
        result
    }

    pub fn stop(&mut self, terminal: PtyKey) -> io::Result<bool> {
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
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
        self.terminal_exit_statuses.remove(&terminal);
        Ok(true)
    }

    pub fn list_sessions(&mut self, namespace: StateNamespace) -> io::Result<Vec<SessionInfo>> {
        self.ensure_connected()?;
        let namespace = wire_state_namespace(namespace);
        self.write(&ClientMessage::ListSessions { namespace })?;
        let deadline = Instant::now() + ATTACH_ACK_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for the namespaced session list",
                ));
            }
            let Some(connection) = self.connection.as_ref() else {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "mult-server disconnected while listing sessions",
                ));
            };
            match connection.receiver.recv_timeout(remaining) {
                Ok(ServerMessage::Sessions {
                    namespace: received,
                    sessions,
                }) if received == namespace => return Ok(sessions),
                Ok(message) => self.route_during_request(message),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for the namespaced session list",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.connection = None;
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "mult-server disconnected while listing sessions",
                    ));
                }
            }
        }
    }

    pub fn update_agent_status(
        &mut self,
        record: AgentStatusRecord,
    ) -> io::Result<AgentStatusRecord> {
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
    ) -> io::Result<Option<AgentStatusRecord>> {
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

    pub fn send_input(&mut self, terminal: PtyKey, input: &[u8]) -> io::Result<bool> {
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
            return Ok(false);
        };
        self.send_input_inner(terminal, pane, input, true)?;
        Ok(true)
    }

    fn send_input_inner(
        &mut self,
        terminal: PtyKey,
        pane: PaneId,
        input: &[u8],
        track_command: bool,
    ) -> io::Result<()> {
        if !input.is_empty() {
            let alternate_screen = self
                .parsers
                .get(&terminal)
                .is_some_and(|parser| parser.screen().alternate_screen());
            if let Some(parser) = self.parsers.get_mut(&terminal) {
                parser.set_scrollback(0);
            }
            if track_command && !alternate_screen && self.terminal_accepts_shell_input(terminal) {
                self.command_trackers
                    .entry(terminal)
                    .or_default()
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
        let Some(process) = self.foreground_processes.get(&terminal) else {
            return true;
        };
        match (process.root_pid, process.foreground_pid) {
            (Some(root_pid), Some(foreground_pid)) => root_pid == foreground_pid,
            _ => true,
        }
    }

    pub fn send_paste(&mut self, terminal: PtyKey, text: &str) -> io::Result<bool> {
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
            return Ok(false);
        };
        let use_bracketed = self
            .parsers
            .get(&terminal)
            .is_some_and(|parser| parser.screen().bracketed_paste());
        let bytes = terminal_paste_bytes(text, use_bracketed);
        if !bytes.is_empty() {
            let alternate_screen = self
                .parsers
                .get(&terminal)
                .is_some_and(|parser| parser.screen().alternate_screen());
            if let Some(parser) = self.parsers.get_mut(&terminal) {
                parser.set_scrollback(0);
            }
            if !alternate_screen && self.terminal_accepts_shell_input(terminal) {
                self.command_trackers
                    .entry(terminal)
                    .or_default()
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
    /// reporting. When it has, the wheel belongs to the program (it scrolls its
    /// own view) rather than to our local scrollback — which for an
    /// alternate-screen app like Claude Code holds nothing to scroll anyway.
    pub fn terminal_reports_mouse(&self, terminal: PtyKey) -> bool {
        self.parsers
            .get(&terminal)
            .is_some_and(|parser| parser.screen().mouse_protocol_mode() != MouseProtocolMode::None)
    }

    /// Forward one scroll-wheel notch to a mouse-reporting program, encoded in
    /// the protocol it requested. `col`/`row` are 1-based, screen-relative cell
    /// coordinates. Returns false when the terminal has no live parser/pane or
    /// is not reporting the mouse.
    pub fn forward_wheel(&mut self, terminal: PtyKey, up: bool, col: u16, row: u16) -> bool {
        let Some(parser) = self.parsers.get(&terminal) else {
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
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
            return false;
        };
        match self.send_input_inner(terminal, pane, &bytes, false) {
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

    pub fn scroll_up(&mut self, terminal: PtyKey, rows: usize) -> io::Result<bool> {
        Ok(self.scroll_parser(terminal, rows as i32))
    }

    pub fn scroll_down(&mut self, terminal: PtyKey, rows: usize) -> io::Result<bool> {
        Ok(self.scroll_parser(terminal, -(rows.min(i32::MAX as usize) as i32)))
    }

    pub fn scroll_to_top(&mut self, terminal: PtyKey) -> io::Result<bool> {
        let Some(parser) = self.parsers.get_mut(&terminal) else {
            return Ok(false);
        };
        let old = parser.screen().scrollback();
        parser.set_scrollback(TERMINAL_SCROLLBACK_LINES);
        clamp_parser_scrollback(parser);
        Ok(parser.screen().scrollback() != old)
    }

    pub fn scroll_to_bottom(&mut self, terminal: PtyKey) -> io::Result<bool> {
        let Some(parser) = self.parsers.get_mut(&terminal) else {
            return Ok(false);
        };
        let old = parser.screen().scrollback();
        parser.set_scrollback(0);
        Ok(old != 0)
    }

    pub fn resize(&mut self, terminal: PtyKey, size: PtyDimensions) -> io::Result<()> {
        self.resize_parser(terminal, size);
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
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

    pub fn drain_events(&mut self) -> Vec<PtyEvent> {
        let mut events = std::mem::take(&mut self.pending_events);
        let was_connected = self.connection.is_some();
        while let Some(connection) = self.connection.as_ref() {
            match connection.receiver.try_recv() {
                Ok(message) => self.handle_server_message(message, &mut events),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.connection = None;
                    break;
                }
            }
        }
        if was_connected && self.connection.is_none() {
            self.reconnect_or_report();
            events.append(&mut self.pending_events);
        }
        events
    }

    fn allocate_request(&mut self) -> io::Result<RequestId> {
        if self.pending_requests.len() >= MAX_PENDING_REQUESTS_PER_CLIENT {
            return Err(io::Error::other("too many pending PTY requests"));
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

    fn perform_create(&mut self, request_id: RequestId, request: ClientMessage) -> io::Result<()> {
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
                Err(error) if is_disconnected_error(&error) => {
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
                ServerMessage::Error { message } => return Err(io::Error::other(message)),
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
    ) -> io::Result<()> {
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
                Err(error) if is_disconnected_error(&error) => {
                    self.resume_and_resend(&request)?;
                    self.reset_parser(terminal, size);
                    self.pane_leases.remove(&pane_id);
                    self.expected_output.remove(&pane_id);
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
                        bytes: std::mem::take(&mut replay_bytes),
                    });
                    if replay_omitted > 0 {
                        self.pending_events.push(PtyEvent::ReplayTruncated {
                            terminal,
                            omitted_bytes: replay_omitted,
                        });
                    }
                    self.pane_leases.insert(pane_id, lease);
                    self.expected_output.insert(pane_id, watermark);
                    return Ok(());
                }
                ServerMessage::PtyOutput { pane, lease, .. }
                    if pane == pane_id && accepted_lease == Some(lease) =>
                {
                    return Err(protocol_order_error(
                        "live output arrived before replay end",
                    ));
                }
                ServerMessage::Error { message } => return Err(io::Error::other(message)),
                message => self.route_during_request(message),
            }
        }
    }

    fn perform_agent_status(
        &mut self,
        request_id: RequestId,
        request: ClientMessage,
    ) -> io::Result<AgentStatusOutcome> {
        self.write_idempotent_request(&request)?;
        let deadline = Instant::now() + ATTACH_ACK_TIMEOUT;
        loop {
            let message = match self.receive_for_request(request_id, deadline) {
                Ok(message) => message,
                Err(error) if is_disconnected_error(&error) => {
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
                ServerMessage::Error { message } => return Err(io::Error::other(message)),
                message => self.route_during_request(message),
            }
        }
    }

    fn perform_stop(&mut self, request_id: RequestId, request: ClientMessage) -> io::Result<()> {
        let (stop_pane, stop_lease) = match &request {
            ClientMessage::Stop { pane, lease, .. } => (*pane, *lease),
            _ => unreachable!("perform_stop requires Stop"),
        };
        self.write_idempotent_request(&request)?;
        let deadline = Instant::now() + STOP_ACK_TIMEOUT;
        loop {
            let message = match self.receive_for_request(request_id, deadline) {
                Ok(message) => message,
                Err(error) if is_disconnected_error(&error) => {
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
                ServerMessage::Error { message } => return Err(io::Error::other(message)),
                message => self.route_during_request(message),
            }
        }
    }

    fn receive_for_request(
        &mut self,
        request_id: RequestId,
        deadline: Instant,
    ) -> io::Result<ServerMessage> {
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
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for correlated mult-server response",
                ));
            }
            let Some(connection) = self.connection.as_ref() else {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "not connected to mult-server",
                ));
            };
            match connection.receiver.recv_timeout(remaining) {
                Ok(message) if message_request_id(&message).is_some_and(|id| id != request_id) => {
                    self.deferred_messages.push_back(message);
                }
                Ok(message) => return Ok(message),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for correlated mult-server response",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.connection = None;
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "mult-server disconnected before correlated response",
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

    fn resize_parser(&mut self, terminal: PtyKey, size: PtyDimensions) {
        let parser = self
            .parsers
            .entry(terminal)
            .or_insert_with(|| Parser::new(24, 80, TERMINAL_SCROLLBACK_LINES));
        parser.set_size(size.rows.max(1), size.cols.max(1));
        clamp_parser_scrollback(parser);
    }

    fn scroll_parser(&mut self, terminal: PtyKey, rows: i32) -> bool {
        if rows == 0 {
            return false;
        }
        let Some(parser) = self.parsers.get_mut(&terminal) else {
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
                if self.pane_leases.get(&pane) == Some(&lease) {
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
                if self.pane_leases.get(&pane) != Some(&lease) {
                    return;
                }
                let Some(expected) = self.expected_output.get(&pane).copied() else {
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
                self.expected_output.insert(pane, next);
                if let Some(terminal) = self.key_for_pane(pane) {
                    self.feed_terminal_output(terminal, &bytes, true);
                    events.push(PtyEvent::Output { terminal, bytes });
                }
            }
            ServerMessage::PaneExited { pane, lease, exit } => {
                if self.pane_leases.get(&pane) != Some(&lease) {
                    return;
                }
                if let Some(terminal) = self.clear_attachment(pane) {
                    let status = PtyExit {
                        code: exit.code,
                        signal: exit.signal,
                    };
                    self.terminal_exit_statuses.insert(terminal, status.clone());
                    events.push(PtyEvent::Exited { terminal, status });
                }
            }
            ServerMessage::TakenOver { pane, lease } => {
                if self.pane_leases.get(&pane) == Some(&lease) {
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
                if self.pane_leases.get(&pane) == Some(&lease) {
                    let terminal = self
                        .clear_attachment(pane)
                        .unwrap_or_else(|| key_for_pane_id(pane));
                    events.push(PtyEvent::Error {
                        terminal,
                        message: format!(
                            "{:?} rejected for pane {}: {:?}",
                            operation, pane.0, reason
                        ),
                    });
                }
            }
            ServerMessage::Error { message } => {
                let terminal = self
                    .pane_to_terminal
                    .values()
                    .next()
                    .copied()
                    .unwrap_or(PtyKey::Terminal(TerminalId(0)));
                events.push(PtyEvent::Error { terminal, message });
            }
        }
    }

    fn mark_attachment_unreconciled(
        &mut self,
        pane: PaneId,
        events: &mut Vec<PtyEvent>,
        message: String,
    ) {
        self.pane_leases.remove(&pane);
        self.expected_output.remove(&pane);
        if let Some(terminal) = self.key_for_pane(pane) {
            events.push(PtyEvent::Error { terminal, message });
        }
    }

    fn clear_attachment(&mut self, pane: PaneId) -> Option<PtyKey> {
        self.pane_leases.remove(&pane);
        self.expected_output.remove(&pane);
        let terminal = self.pane_to_terminal.remove(&pane)?;
        self.terminal_to_pane.remove(&terminal);
        Some(terminal)
    }

    fn identity_for_key(&self, terminal: PtyKey) -> io::Result<WireSessionIdentity> {
        if let Some(identity) = self.session_identities.get(&terminal) {
            return Ok(*identity);
        }
        #[cfg(test)]
        {
            Ok(test_wire_session_identity(terminal))
        }
        #[cfg(not(test))]
        {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("PTY session {terminal:?} has no registered durable identity"),
            ))
        }
    }

    fn lease_for_pane(&self, pane: PaneId) -> io::Result<AttachmentLease> {
        self.pane_leases.get(&pane).copied().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("pane {} attachment has not been reconciled", pane.0),
            )
        })
    }

    fn key_for_pane(&self, pane: PaneId) -> Option<PtyKey> {
        self.pane_to_terminal.get(&pane).copied()
    }

    fn record_foreground_process(&mut self, terminal: PtyKey, process: ForegroundProcessInfo) {
        let foreground_is_child = matches!(
            (process.root_pid, process.foreground_pid),
            (Some(root_pid), Some(foreground_pid)) if root_pid != foreground_pid
        );
        if foreground_is_child {
            if let Some(command) = process.command.as_deref() {
                self.command_trackers
                    .entry(terminal)
                    .or_default()
                    .record_process_command(command);
            }
        }
        self.foreground_processes.insert(terminal, process);
    }

    fn ensure_connected(&mut self) -> io::Result<()> {
        if self.connection.is_some() {
            return Ok(());
        }
        self.connect_inner(true)?;
        self.reattach_terminals()
    }

    fn reconnect_or_report(&mut self) {
        let result = self
            .connect_inner(false)
            .and_then(|_| self.reattach_terminals());
        if let Err(error) = result {
            let terminal = self
                .terminal_to_pane
                .keys()
                .next()
                .copied()
                .unwrap_or(PtyKey::Terminal(TerminalId(0)));
            self.pending_events.push(PtyEvent::Error {
                terminal,
                message: format!(
                    "mult-server connection lost; attachment state is retained pending reconciliation: {error}"
                ),
            });
        }
    }

    fn reattach_terminals(&mut self) -> io::Result<()> {
        let terminals = self
            .terminal_to_pane
            .keys()
            .copied()
            .filter(|terminal| Some(*terminal) != self.starting)
            .collect::<Vec<_>>();
        for terminal in terminals {
            let size = self.parser_dimensions(terminal);
            self.reset_parser(terminal, size);
            let identity = self.identity_for_key(terminal)?;
            let session = session_for_key(terminal);
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
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let pane = pane_for_key(terminal);
                    self.clear_attachment(pane);
                    let status = PtyExit {
                        code: 1,
                        signal: Some("server session unavailable".to_string()),
                    };
                    self.terminal_exit_statuses.insert(terminal, status.clone());
                    self.pending_events
                        .push(PtyEvent::Exited { terminal, status });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn parser_dimensions(&self, terminal: PtyKey) -> PtyDimensions {
        self.parsers
            .get(&terminal)
            .map(|parser| {
                let (rows, cols) = parser.screen().size();
                PtyDimensions { rows, cols }
            })
            .unwrap_or_default()
    }

    fn connect(&mut self) -> io::Result<()> {
        self.connect_inner(true).map(|_| ())
    }

    /// Connect and return whether the daemon resumed the exact previous
    /// client-scope/server-instance pair.
    fn connect_inner(&mut self, allow_spawn: bool) -> io::Result<bool> {
        let mut stream = if allow_spawn {
            connect_or_spawn_server(&self.socket_path)?
        } else {
            UnixStream::connect(&self.socket_path)?
        };
        validate_peer_owner(&stream, "mult-server")?;
        stream.set_nonblocking(false)?;
        let mut writer_stream = stream.try_clone()?;
        write_message(
            &mut writer_stream,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                resume: self.client_scope,
            },
        )?;
        let hello = validate_server_hello_with_timeout(&mut stream, SERVER_HELLO_TIMEOUT)?;
        let resumed_same = hello.resumed
            && self.client_scope == Some(hello.client_scope)
            && self.server_instance == Some(hello.server_instance);
        if self.client_scope.is_some() && !resumed_same {
            self.pending_requests.clear();
            self.deferred_messages.clear();
            self.next_request_id = Some(RequestId::MIN);
            self.pane_leases.clear();
            self.expected_output.clear();
        }
        self.client_scope = Some(hello.client_scope);
        self.server_instance = Some(hello.server_instance);

        let writer = Arc::new(Mutex::new(writer_stream));
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
                            message: format!("failed to read from mult-server: {error}"),
                        });
                        break;
                    }
                }
            }
        });
        self.connection = Some(ServerConnection { writer, receiver });
        Ok(resumed_same)
    }

    fn write_idempotent_request(&mut self, message: &ClientMessage) -> io::Result<()> {
        self.ensure_connected()?;
        match self.write(message) {
            Ok(()) => Ok(()),
            Err(error) if is_disconnected_error(&error) => self.resume_and_resend(message),
            Err(error) => Err(error),
        }
    }

    fn resume_and_resend(&mut self, message: &ClientMessage) -> io::Result<()> {
        let previous_scope = self.client_scope;
        let previous_instance = self.server_instance;
        self.connection = None;
        let resumed_same = self.connect_inner(true)?;
        if !resumed_same
            || self.client_scope != previous_scope
            || self.server_instance != previous_instance
        {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "mult-server identity changed; refusing to replay an unresolved request",
            ));
        }
        self.write(message)
    }

    fn write_non_replayable(
        &mut self,
        message: &ClientMessage,
        operation: PtyDeliveryOperation,
        pane: PaneId,
    ) -> io::Result<()> {
        let Some(connection) = &self.connection else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "not connected to mult-server",
            ));
        };
        let result = {
            let mut writer = connection
                .writer
                .lock()
                .map_err(|_| io::Error::other("server socket writer lock poisoned"))?;
            write_non_replayable_frame(&mut *writer, message, operation, pane)
        };
        if result.as_ref().is_err_and(|error| {
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<PtyDeliveryError>())
                .is_some()
        }) {
            self.connection = None;
        }
        result
    }

    fn write(&self, message: &ClientMessage) -> io::Result<()> {
        let Some(connection) = &self.connection else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "not connected to mult-server",
            ));
        };
        let mut writer = connection
            .writer
            .lock()
            .map_err(|_| io::Error::other("server socket writer lock poisoned"))?;
        write_message(&mut *writer, message)
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
) -> io::Result<()> {
    let mut tracked = AttemptTrackingWriter {
        inner: writer,
        attempted: false,
    };
    match write_message(&mut tracked, message) {
        Ok(()) => Ok(()),
        Err(error) if !tracked.attempted => Err(error),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            PtyDeliveryError { operation, pane },
        )),
    }
}

impl Drop for PtyRuntime {
    fn drop(&mut self) {
        let Some(connection) = &self.connection else {
            return;
        };
        if let Ok(mut writer) = connection.writer.lock() {
            for (pane, lease) in &self.pane_leases {
                let _ = write_message(
                    &mut *writer,
                    &ClientMessage::Detach {
                        pane: *pane,
                        lease: *lease,
                    },
                );
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

    fn advance(&mut self, byte: u8, screen: &vt100::Screen) -> Option<Vec<u8>> {
        let state = std::mem::take(&mut self.state);
        let (next, response) = match state {
            TerminalResponseState::Ground => match byte {
                0x1b => (TerminalResponseState::Escape, None),
                _ => (TerminalResponseState::Ground, None),
            },
            TerminalResponseState::Escape => match byte {
                b'[' => (TerminalResponseState::Csi(Vec::new()), None),
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    (TerminalResponseState::String { esc_seen: false }, None)
                }
                b'(' | b')' | b'*' | b'+' => (TerminalResponseState::IgnoreOne, None),
                b'Z' => (
                    TerminalResponseState::Ground,
                    Some(PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.to_vec()),
                ),
                _ => (TerminalResponseState::Ground, None),
            },
            TerminalResponseState::Csi(mut sequence) => {
                if (0x40..=0x7e).contains(&byte) {
                    let response = csi_terminal_response(&sequence, byte as char, screen);
                    (TerminalResponseState::Ground, response)
                } else if sequence.len() >= TERMINAL_MAX_CSI_SEQUENCE_BYTES {
                    (TerminalResponseState::CsiIgnored, None)
                } else {
                    sequence.push(byte);
                    (TerminalResponseState::Csi(sequence), None)
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

/// Feed `bytes` to `parser` while letting `responder` answer terminal queries.
/// While the responder is idle (Ground) no query can begin until an escape, so
/// the run of bytes up to the next ESC is fed in a single `process` call; only
/// escape sequences are stepped one byte at a time, which is what the responder
/// needs to report state such as the cursor position at the exact query point.
/// This is behaviourally identical to feeding every byte individually.
fn feed_parser_with_responder(
    parser: &mut Parser,
    responder: &mut TerminalResponseDetector,
    bytes: &[u8],
) -> Vec<Vec<u8>> {
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
        let byte = bytes[index];
        parser.process(std::slice::from_ref(&byte));
        if let Some(response) = responder.advance(byte, parser.screen()) {
            responses.push(response);
        }
        index += 1;
    }
    responses
}

fn csi_terminal_response(
    sequence: &[u8],
    final_char: char,
    screen: &vt100::Screen,
) -> Option<Vec<u8>> {
    let private = sequence.contains(&b'?');
    let params = parse_csi_params(sequence);
    match final_char {
        'c' if !private && param_or_default(&params, 0, 0) == 0 => {
            Some(PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.to_vec())
        }
        'n' if !private => match param_or_default(&params, 0, 0) {
            5 => Some(DEVICE_STATUS_OK_RESPONSE.to_vec()),
            6 => Some(cursor_position_report(screen, false)),
            _ => None,
        },
        'n' if private && param_or_default(&params, 0, 0) == 6 => {
            Some(cursor_position_report(screen, true))
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
) -> io::Result<ServerHello> {
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

fn validate_server_hello(reader: &mut impl io::Read) -> io::Result<ServerHello> {
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
        ServerMessage::Hello {
            protocol_version, ..
        } => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "mult-server protocol version {protocol_version} is incompatible with client version {PROTOCOL_VERSION}; restart mult-server"
            ),
        )),
        ServerMessage::Error { message } => Err(io::Error::new(io::ErrorKind::InvalidData, message)),
        message => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected mult-server hello response: {message:?}"),
        )),
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

fn protocol_order_error(message: &str) -> io::Error {
    // Keep the terminal/session mapping but mark this attach unreconciled. A
    // later explicit attach can rebuild from an authoritative replay.
    io::Error::new(io::ErrorKind::ConnectionAborted, message)
}

fn create_error(error: CreateError) -> io::Error {
    match error {
        CreateError::SessionAlreadyExists { session }
        | CreateError::IdentityAlreadyExists { session } => io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("session {} already exists", session.id.0),
        ),
        CreateError::IdentityMismatch { session, mismatch } => io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("session {} identity mismatch: {mismatch:?}", session.0),
        ),
        CreateError::InvalidAgentMetadata(error) => agent_status_error(error),
        CreateError::RequestCollision => {
            io::Error::new(io::ErrorKind::InvalidData, "create request ID collision")
        }
        CreateError::RetryExpired => {
            io::Error::new(io::ErrorKind::InvalidData, "create request retry expired")
        }
        CreateError::Failed { message } => io::Error::other(message),
    }
}

fn attach_error(error: AttachError) -> io::Error {
    match error {
        AttachError::SessionNotFound { session } => io::Error::new(
            io::ErrorKind::NotFound,
            format!("server session {} is unavailable", session.0),
        ),
        AttachError::IdentityMismatch { session, mismatch } => io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "server session {} identity mismatch: {mismatch:?}",
                session.0
            ),
        ),
        AttachError::Superseded => io::Error::new(
            io::ErrorKind::AlreadyExists,
            "attachment request was superseded by a takeover",
        ),
        AttachError::RequestCollision => {
            io::Error::new(io::ErrorKind::InvalidData, "attach request ID collision")
        }
        AttachError::RetryExpired => {
            io::Error::new(io::ErrorKind::InvalidData, "attach request retry expired")
        }
        AttachError::Failed { message } => io::Error::other(message),
    }
}

fn stop_error(error: StopError) -> io::Error {
    match error {
        StopError::RequestCollision => {
            io::Error::new(io::ErrorKind::InvalidData, "stop request ID collision")
        }
        StopError::RetryExpired => {
            io::Error::new(io::ErrorKind::InvalidData, "stop request retry expired")
        }
        StopError::IdentityMismatch { pane, mismatch } => io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("pane {} identity mismatch: {mismatch:?}", pane.0),
        ),
        StopError::LeaseRejected(reason) => {
            let kind = if reason == LeaseRejectionReason::PaneMissing {
                io::ErrorKind::NotFound
            } else {
                io::ErrorKind::PermissionDenied
            };
            io::Error::new(kind, format!("stop lease rejected: {reason:?}"))
        }
        StopError::Failed { message } => io::Error::other(message),
    }
}

fn agent_status_error(error: AgentStatusError) -> io::Error {
    let kind = match error {
        AgentStatusError::SessionNotFound { .. } => io::ErrorKind::NotFound,
        AgentStatusError::IdentityMismatch(_)
        | AgentStatusError::NotAgentSession { .. }
        | AgentStatusError::WrongChat { .. }
        | AgentStatusError::WrongAgent { .. }
        | AgentStatusError::StaleGeneration { .. }
        | AgentStatusError::FinalStatusConflict { .. } => io::ErrorKind::PermissionDenied,
        AgentStatusError::RequestCollision
        | AgentStatusError::RetryExpired
        | AgentStatusError::WrongSchema { .. } => io::ErrorKind::InvalidData,
        AgentStatusError::Failed { .. } => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("agent status rejected: {error:?}"))
}

fn is_reconciliation_uncertain(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
    )
}

fn validate_peer_owner(stream: &UnixStream, peer_label: &str) -> io::Result<()> {
    let Some(peer_uid) = peer_uid(stream)? else {
        return Ok(());
    };
    let current_uid = current_euid();
    if uid_matches_peer(peer_uid, current_uid) {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("rejecting {peer_label} uid {peer_uid}; expected current uid {current_uid}"),
    ))
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

fn uid_matches_peer(peer_uid: u32, current_uid: u32) -> bool {
    peer_uid == current_uid
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
    command
        .env(SOCKET_PATH_ENV, socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_autospawned_server(&mut command);
    command.spawn().map(|_| ())
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
    Some(path)
}

fn server_executable_name() -> &'static str {
    if cfg!(windows) {
        "mult-server.exe"
    } else {
        "mult-server"
    }
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

fn shell_command_args(command: String) -> Vec<String> {
    // The command string is handed to the login shell for evaluation (`-lc`),
    // so it is fully shell-interpreted: pipelines, `$VAR` expansion, and globbing
    // all apply. This is by design for `pi_agent_command` and
    // `TerminalLaunch::Command`, and is the deliberate difference from
    // `MULT_AGENT_CMD`, which `mult` splits into argv with no shell. See AGENTS.md.
    #[cfg(windows)]
    {
        vec!["-NoExit".to_string(), "-Command".to_string(), command]
    }

    #[cfg(not(windows))]
    {
        vec!["-lc".to_string(), command]
    }
}

fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
    }

    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
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
        let mut runtime = PtyRuntime {
            socket_path: unique_socket_path(),
            connection: Some(ServerConnection {
                writer: Arc::new(Mutex::new(client_stream)),
                receiver,
            }),
            terminal_to_pane: HashMap::from([(terminal, pane)]),
            pane_to_terminal: HashMap::from([(pane, terminal)]),
            pane_leases: HashMap::from([(pane, test_lease())]),
            expected_output: HashMap::from([(pane, OutputSequence::ZERO)]),
            session_identities: HashMap::from([(terminal, test_wire_session_identity(terminal))]),
            agent_sessions: HashMap::new(),
            parsers: HashMap::new(),
            responders: HashMap::new(),
            terminals_with_output: HashSet::new(),
            terminal_exit_statuses: HashMap::new(),
            foreground_processes: HashMap::new(),
            command_trackers: HashMap::new(),
            pending_events: Vec::new(),
            client_scope: Some(test_scope()),
            server_instance: Some(test_server_instance()),
            next_request_id: Some(RequestId::MIN),
            pending_requests: HashSet::new(),
            deferred_messages: VecDeque::new(),
            starting: None,
        };
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
    fn peer_owner_check_accepts_same_user_socket_pair() {
        let (client, _server) = UnixStream::pair().expect("create socket pair");

        validate_peer_owner(&client, "test peer").expect("same uid peer is accepted");
        assert!(uid_matches_peer(current_euid(), current_euid()));
        assert!(!uid_matches_peer(
            current_euid().saturating_add(1),
            current_euid()
        ));
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
            for (sequence, bytes) in [(5, b"abc".to_vec()), (8, b"def".to_vec())] {
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
                    bytes: b"!".to_vec(),
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
                bytes,
            } if *event_terminal == terminal && bytes == b"!"
        )));
        assert!(runtime.terminal_lines(terminal)[0].contains("abcdef!"));
        assert_eq!(
            runtime.expected_output.get(&pane),
            Some(&OutputSequence::new(12))
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
        let mut runtime = PtyRuntime {
            socket_path: unique_socket_path(),
            connection: Some(ServerConnection {
                writer: Arc::new(Mutex::new(client_stream)),
                receiver,
            }),
            terminal_to_pane: HashMap::new(),
            pane_to_terminal: HashMap::new(),
            pane_leases: HashMap::new(),
            expected_output: HashMap::new(),
            session_identities: HashMap::new(),
            agent_sessions: HashMap::new(),
            parsers: HashMap::new(),
            responders: HashMap::new(),
            terminals_with_output: HashSet::new(),
            terminal_exit_statuses: HashMap::new(),
            foreground_processes: HashMap::new(),
            command_trackers: HashMap::new(),
            pending_events: Vec::new(),
            client_scope: Some(test_scope()),
            server_instance: Some(test_server_instance()),
            next_request_id: Some(RequestId::MIN),
            pending_requests: HashSet::new(),
            deferred_messages: VecDeque::new(),
            starting: None,
        };
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

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!runtime.is_running(terminal));
        assert!(!runtime.pane_to_terminal.contains_key(&pane));
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
        assert!(!runtime.pane_to_terminal.contains_key(&pane));
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
                    message: "failed to kill child".to_string(),
                }),
            })
            .expect("send stop rejection");

        let error = runtime.stop(terminal).expect_err("stop should fail");

        assert_eq!(error.to_string(), "failed to kill child");
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
        assert_eq!(runtime.pane_to_terminal.get(&pane), Some(&terminal));
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
        let terminal = PtyKey::Terminal(TerminalId(7));
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 24, cols: 80 });
        // Claude Code's startup: enter the alternate screen and request SGR
        // mouse reporting. After this the program owns the wheel.
        runtime.process_terminal_output(terminal, b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        assert!(runtime.terminal_reports_mouse(terminal));

        assert!(runtime.forward_wheel(terminal, true, 12, 5));

        let message = read_client_message(&mut server_stream, "reading forwarded wheel");
        assert_eq!(
            message,
            ClientMessage::Input {
                pane,
                lease: test_lease(),
                bytes: b"\x1b[<64;12;5M".to_vec(),
            }
        );
    }

    #[test]
    fn wheel_is_not_forwarded_when_the_program_ignores_the_mouse() {
        let mut runtime = PtyRuntime::new_offline();
        let terminal = PtyKey::Terminal(TerminalId(7));
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree");

        assert!(!runtime.terminal_reports_mouse(terminal));
        assert!(!runtime.forward_wheel(terminal, true, 1, 1));
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
            .scroll_to_top(PtyKey::Terminal(TerminalId(99)))
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
        let mut runtime = PtyRuntime {
            socket_path: unique_socket_path(),
            connection: Some(ServerConnection { writer, receiver }),
            terminal_to_pane: HashMap::from([(terminal, pane)]),
            pane_to_terminal: HashMap::from([(pane, terminal)]),
            pane_leases: HashMap::from([(pane, test_lease())]),
            expected_output: HashMap::from([(pane, OutputSequence::ZERO)]),
            session_identities: HashMap::from([(terminal, test_wire_session_identity(terminal))]),
            agent_sessions: HashMap::new(),
            parsers: HashMap::new(),
            responders: HashMap::new(),
            terminals_with_output: HashSet::new(),
            terminal_exit_statuses: HashMap::new(),
            foreground_processes: HashMap::new(),
            command_trackers: HashMap::new(),
            pending_events: Vec::new(),
            client_scope: Some(test_scope()),
            server_instance: Some(test_server_instance()),
            next_request_id: Some(RequestId::MIN),
            pending_requests: HashSet::new(),
            deferred_messages: VecDeque::new(),
            starting: None,
        };

        let error = runtime.stop(terminal).expect_err("stop should fail");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(runtime.is_running(terminal));
        assert_eq!(runtime.pane_to_terminal.get(&pane), Some(&terminal));
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
        assert!(!runtime.pane_to_terminal.contains_key(&pane));
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
                bytes: b"\x1b[c".to_vec(),
            })
            .expect("send terminal query");

        let events = runtime.drain_events();
        let message = read_client_message(&mut server_stream, "reading DA response");

        assert_eq!(
            events,
            vec![PtyEvent::Output {
                terminal,
                bytes: b"\x1b[c".to_vec(),
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
                bytes: b"abc\x1b[6n".to_vec(),
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
                bytes: b"hello".to_vec(),
            })
            .expect("send output event");

        let events = runtime.drain_events();

        assert_eq!(
            events,
            vec![PtyEvent::Output {
                terminal,
                bytes: b"hello".to_vec(),
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
        assert_eq!(runtime.pane_to_terminal.get(&pane), Some(&terminal));
        assert!(runtime.parser(terminal).is_some());
        assert!(events.iter().any(|event| matches!(
            event,
            PtyEvent::Error { terminal: event_terminal, message }
                if *event_terminal == terminal && message.contains("retained pending reconciliation")
        )));
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

        runtime.reattach_terminals().expect("reattach");
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

        runtime
            .reattach_terminals()
            .expect("skip starting terminal");

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
                bytes: b"pane-b-output".to_vec(),
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
                bytes,
            } if *event_terminal == terminal && bytes == b"pane-b-output"
        )));
    }

    #[test]
    fn pane_a_attach_error_does_not_abort_pane_b_replay() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal_b = PtyKey::Terminal(TerminalId(32));
        let pane_b = PaneId(32);
        let mut runtime = unattached_test_runtime(client_stream, receiver);
        runtime.terminal_to_pane.insert(terminal_b, pane_b);
        runtime.pane_to_terminal.insert(pane_b, terminal_b);
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
                bytes: b"b".to_vec(),
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
        runtime
            .terminal_to_pane
            .insert(second_terminal, second_pane);
        runtime
            .pane_to_terminal
            .insert(second_pane, second_terminal);
        runtime.pane_leases.insert(second_pane, second_lease);
        runtime
            .expected_output
            .insert(second_pane, OutputSequence::ZERO);
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
        assert_eq!(runtime.pane_leases.get(&second_pane), Some(&second_lease));
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
                bytes: b"gap".to_vec(),
            },
            &mut events,
        );

        assert!(!runtime.is_running(terminal));
        assert_eq!(runtime.terminal_to_pane.get(&terminal), Some(&pane));
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
            .get_ref()
            .and_then(|error| error.downcast_ref::<PtyDeliveryError>())
            .is_some_and(|error| error.operation == PtyDeliveryOperation::Input));
        assert!(runtime.connection.is_none());
        assert!(runtime.is_running(terminal));
        assert_eq!(runtime.pane_leases.get(&pane), Some(&test_lease()));
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
                .get_ref()
                .and_then(|source| source.downcast_ref::<PtyDeliveryError>())
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

    fn unattached_test_runtime(
        client_stream: UnixStream,
        receiver: Receiver<ServerMessage>,
    ) -> PtyRuntime {
        PtyRuntime {
            socket_path: unique_socket_path(),
            connection: Some(ServerConnection {
                writer: Arc::new(Mutex::new(client_stream)),
                receiver,
            }),
            terminal_to_pane: HashMap::new(),
            pane_to_terminal: HashMap::new(),
            pane_leases: HashMap::new(),
            expected_output: HashMap::new(),
            session_identities: HashMap::new(),
            agent_sessions: HashMap::new(),
            parsers: HashMap::new(),
            responders: HashMap::new(),
            terminals_with_output: HashSet::new(),
            terminal_exit_statuses: HashMap::new(),
            foreground_processes: HashMap::new(),
            command_trackers: HashMap::new(),
            pending_events: Vec::new(),
            client_scope: Some(test_scope()),
            server_instance: Some(test_server_instance()),
            next_request_id: Some(RequestId::MIN),
            pending_requests: HashSet::new(),
            deferred_messages: VecDeque::new(),
            starting: None,
        }
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
        runtime.terminal_to_pane = HashMap::from([(terminal, pane)]);
        runtime.pane_to_terminal = HashMap::from([(pane, terminal)]);
        runtime.pane_leases = HashMap::from([(pane, test_lease())]);
        runtime.expected_output = HashMap::from([(pane, OutputSequence::ZERO)]);
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
