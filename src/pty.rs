use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs, io,
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
    default_socket_path, read_message, write_message, ClientMessage, ForegroundProcessInfo,
    LaunchSpec, PaneId, ServerMessage, SessionId, PROTOCOL_VERSION, SOCKET_PATH_ENV,
};
use vt100::Parser;

use crate::model::TerminalId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtySpawn {
    pub terminal: TerminalId,
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
        terminal: TerminalId,
        bytes: Vec<u8>,
    },
    Output {
        terminal: TerminalId,
        bytes: Vec<u8>,
    },
    Exited {
        terminal: TerminalId,
        status: PtyExit,
    },
    Error {
        terminal: TerminalId,
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
    connection: Option<ServerConnection>,
    terminal_to_pane: HashMap<TerminalId, PaneId>,
    pane_to_terminal: HashMap<PaneId, TerminalId>,
    parsers: HashMap<TerminalId, Parser>,
    responders: HashMap<TerminalId, TerminalResponseDetector>,
    terminals_with_output: HashSet<TerminalId>,
    terminal_exit_statuses: HashMap<TerminalId, PtyExit>,
    foreground_processes: HashMap<TerminalId, ForegroundProcessInfo>,
    command_trackers: HashMap<TerminalId, TerminalCommandTracker>,
    pending_events: Vec<PtyEvent>,
}

const SERVER_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const TERMINAL_SCROLLBACK_LINES: usize = 5_000;
const TERMINAL_MAX_CSI_SEQUENCE_BYTES: usize = 128;
const PRIMARY_DEVICE_ATTRIBUTES_RESPONSE: &[u8] = b"\x1b[?1;2c";
const DEVICE_STATUS_OK_RESPONSE: &[u8] = b"\x1b[0n";
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

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
                    terminal: TerminalId(0),
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
            parsers: HashMap::new(),
            responders: HashMap::new(),
            terminals_with_output: HashSet::new(),
            terminal_exit_statuses: HashMap::new(),
            foreground_processes: HashMap::new(),
            command_trackers: HashMap::new(),
            pending_events,
        }
    }
}

impl PtySpawn {
    pub fn shell(
        terminal: TerminalId,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
    ) -> Self {
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
        terminal: TerminalId,
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
    pub fn is_running(&self, terminal: TerminalId) -> bool {
        self.terminal_to_pane.contains_key(&terminal)
    }

    pub fn parser(&self, terminal: TerminalId) -> Option<&Parser> {
        self.parsers.get(&terminal)
    }

    pub fn terminal_exit_status(&self, terminal: TerminalId) -> Option<&PtyExit> {
        self.terminal_exit_statuses.get(&terminal)
    }

    pub fn terminal_last_command(&self, terminal: TerminalId) -> Option<&str> {
        self.command_trackers
            .get(&terminal)
            .and_then(TerminalCommandTracker::last_command)
    }

    #[cfg(test)]
    pub fn mark_running_for_test(&mut self, terminal: TerminalId) {
        let pane = pane_for_terminal(terminal);
        self.terminal_to_pane.insert(terminal, pane);
        self.pane_to_terminal.insert(pane, terminal);
    }

    #[cfg(test)]
    pub fn record_exit_status_for_test(&mut self, terminal: TerminalId, status: PtyExit) {
        self.terminal_exit_statuses.insert(terminal, status);
    }

    pub fn ensure_parser(&mut self, terminal: TerminalId, size: PtyDimensions) {
        self.parsers.entry(terminal).or_insert_with(|| {
            Parser::new(
                size.rows.max(1),
                size.cols.max(1),
                TERMINAL_SCROLLBACK_LINES,
            )
        });
        self.resize_parser(terminal, size);
    }

    pub fn reset_parser(&mut self, terminal: TerminalId, size: PtyDimensions) {
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

    pub fn remove_terminal(&mut self, terminal: TerminalId) {
        if let Some(pane) = self.terminal_to_pane.remove(&terminal) {
            self.pane_to_terminal.remove(&pane);
        }
        self.parsers.remove(&terminal);
        self.responders.remove(&terminal);
        self.terminals_with_output.remove(&terminal);
        self.terminal_exit_statuses.remove(&terminal);
        self.foreground_processes.remove(&terminal);
        self.command_trackers.remove(&terminal);
    }

    pub fn process_terminal_output(&mut self, terminal: TerminalId, bytes: &[u8]) {
        self.feed_terminal_output(terminal, bytes, false);
    }

    fn feed_terminal_output(&mut self, terminal: TerminalId, bytes: &[u8], respond: bool) {
        if bytes.is_empty() {
            return;
        }

        let responses = {
            let parser = self
                .parsers
                .entry(terminal)
                .or_insert_with(|| Parser::new(24, 80, TERMINAL_SCROLLBACK_LINES));
            let responder = self.responders.entry(terminal).or_default();
            let mut responses = Vec::new();
            for byte in bytes {
                parser.process(std::slice::from_ref(byte));
                if respond {
                    if let Some(response) = responder.advance(*byte, parser.screen()) {
                        responses.push(response);
                    }
                }
            }
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

    pub fn append_terminal_system_line(&mut self, terminal: TerminalId, message: impl AsRef<str>) {
        let line = format!("[mult] {}\r\n", message.as_ref());
        self.process_terminal_output(terminal, line.as_bytes());
    }

    pub fn terminal_lines(&self, terminal: TerminalId) -> Vec<String> {
        let Some(parser) = self.parsers.get(&terminal) else {
            return Vec::new();
        };
        terminal_screen_rows(parser)
    }

    pub fn terminal_all_lines(&self, terminal: TerminalId) -> Vec<String> {
        self.terminal_lines(terminal)
    }

    pub fn terminal_output_is_blank(&self, terminal: TerminalId) -> bool {
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

    pub fn start(&mut self, spawn: PtySpawn) -> io::Result<()> {
        if self.is_running(spawn.terminal) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal already has a server attachment",
            ));
        }

        self.ensure_connected()?;
        self.reset_parser(spawn.terminal, spawn.size);
        self.foreground_processes.remove(&spawn.terminal);
        self.command_trackers.remove(&spawn.terminal);
        let session = session_for_terminal(spawn.terminal);
        let pane = pane_for_terminal(spawn.terminal);
        let launch = launch_spec(&spawn);
        let name = session_name(&spawn, &launch);
        self.terminal_to_pane.insert(spawn.terminal, pane);
        self.pane_to_terminal.insert(pane, spawn.terminal);

        let result = self
            .send(ClientMessage::CreateSession {
                requested_id: Some(session),
                name,
                cwd: spawn.cwd.clone(),
                env: spawn.env.clone(),
                launch,
                rows: spawn.size.rows,
                cols: spawn.size.cols,
            })
            .and_then(|()| {
                self.send(ClientMessage::Attach {
                    session,
                    rows: spawn.size.rows,
                    cols: spawn.size.cols,
                })
            });

        if result.is_err() {
            self.terminal_to_pane.remove(&spawn.terminal);
            self.pane_to_terminal.remove(&pane);
        }
        result
    }

    pub fn stop(&mut self, terminal: TerminalId) -> io::Result<bool> {
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
            return Ok(false);
        };

        self.send(ClientMessage::Stop { pane })?;
        self.terminal_to_pane.remove(&terminal);
        self.pane_to_terminal.remove(&pane);
        self.terminal_exit_statuses.remove(&terminal);
        Ok(true)
    }

    pub fn send_input(&mut self, terminal: TerminalId, input: &[u8]) -> io::Result<bool> {
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
            return Ok(false);
        };
        self.send_input_inner(terminal, pane, input, true)?;
        Ok(true)
    }

    fn send_input_inner(
        &mut self,
        terminal: TerminalId,
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
        self.send(ClientMessage::Input {
            pane,
            bytes: input.to_vec(),
        })?;
        Ok(())
    }

    fn terminal_accepts_shell_input(&self, terminal: TerminalId) -> bool {
        let Some(process) = self.foreground_processes.get(&terminal) else {
            return true;
        };

        match (process.root_pid, process.foreground_pid) {
            (Some(root_pid), Some(foreground_pid)) => root_pid == foreground_pid,
            _ => true,
        }
    }

    pub fn send_paste(&mut self, terminal: TerminalId, text: &str) -> io::Result<bool> {
        let use_bracketed = self
            .parsers
            .get(&terminal)
            .is_some_and(|parser| parser.screen().bracketed_paste());
        let bytes = terminal_paste_bytes(text, use_bracketed);
        self.send_input(terminal, &bytes)
    }

    pub fn scroll_up(&mut self, terminal: TerminalId, rows: usize) -> io::Result<bool> {
        Ok(self.scroll_parser(terminal, rows as i32))
    }

    pub fn scroll_down(&mut self, terminal: TerminalId, rows: usize) -> io::Result<bool> {
        Ok(self.scroll_parser(terminal, -(rows.min(i32::MAX as usize) as i32)))
    }

    pub fn scroll_to_top(&mut self, terminal: TerminalId) -> io::Result<bool> {
        let Some(parser) = self.parsers.get_mut(&terminal) else {
            return Ok(false);
        };
        let old = parser.screen().scrollback();
        parser.set_scrollback(TERMINAL_SCROLLBACK_LINES);
        clamp_parser_scrollback(parser);
        Ok(parser.screen().scrollback() != old)
    }

    pub fn scroll_to_bottom(&mut self, terminal: TerminalId) -> io::Result<bool> {
        let Some(parser) = self.parsers.get_mut(&terminal) else {
            return Ok(false);
        };
        let old = parser.screen().scrollback();
        parser.set_scrollback(0);
        Ok(old != 0)
    }

    pub fn resize(&mut self, terminal: TerminalId, size: PtyDimensions) -> io::Result<()> {
        self.resize_parser(terminal, size);
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
            return Ok(());
        };
        self.send(ClientMessage::Resize {
            pane,
            rows: size.rows,
            cols: size.cols,
        })
    }

    pub fn drain_events(&mut self) -> Vec<PtyEvent> {
        let mut events = std::mem::take(&mut self.pending_events);
        let mut disconnected = self.connection.is_none();

        while self.connection.is_some() {
            let message = self
                .connection
                .as_ref()
                .map(|connection| connection.receiver.try_recv());
            match message {
                Some(Ok(message)) => self.handle_server_message(message, &mut events),
                Some(Err(TryRecvError::Empty)) => break,
                Some(Err(TryRecvError::Disconnected)) | None => {
                    disconnected = true;
                    break;
                }
            }
        }

        if disconnected {
            self.connection = None;
            self.terminal_to_pane.clear();
            self.pane_to_terminal.clear();
        }

        events
    }

    fn resize_parser(&mut self, terminal: TerminalId, size: PtyDimensions) {
        let parser = self
            .parsers
            .entry(terminal)
            .or_insert_with(|| Parser::new(24, 80, TERMINAL_SCROLLBACK_LINES));
        parser.set_size(size.rows.max(1), size.cols.max(1));
        clamp_parser_scrollback(parser);
    }

    fn scroll_parser(&mut self, terminal: TerminalId, rows: i32) -> bool {
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
            ServerMessage::Hello { .. }
            | ServerMessage::Sessions(_)
            | ServerMessage::Attached { .. } => {}
            ServerMessage::ForegroundProcess { pane, process } => {
                if let Some(terminal) = self.terminal_for_pane(pane) {
                    self.record_foreground_process(terminal, process);
                }
            }
            ServerMessage::PtyScrollback { pane, bytes } => {
                if let Some(terminal) = self.terminal_for_pane(pane) {
                    self.feed_terminal_output(terminal, &bytes, false);
                    events.push(PtyEvent::Scrollback { terminal, bytes });
                }
            }
            ServerMessage::PtyOutput { pane, bytes } => {
                if let Some(terminal) = self.terminal_for_pane(pane) {
                    self.feed_terminal_output(terminal, &bytes, true);
                    events.push(PtyEvent::Output { terminal, bytes });
                }
            }
            ServerMessage::PaneExited { pane, exit } => {
                if let Some(terminal) = self.pane_to_terminal.remove(&pane) {
                    self.terminal_to_pane.remove(&terminal);
                    let status = PtyExit {
                        code: exit.code,
                        signal: exit.signal,
                    };
                    self.terminal_exit_statuses.insert(terminal, status.clone());
                    events.push(PtyEvent::Exited { terminal, status });
                }
            }
            ServerMessage::Error { message } => {
                let terminal = self
                    .pane_to_terminal
                    .values()
                    .next()
                    .copied()
                    .unwrap_or(TerminalId(0));
                events.push(PtyEvent::Error { terminal, message });
            }
        }
    }

    fn terminal_for_pane(&self, pane: PaneId) -> Option<TerminalId> {
        self.pane_to_terminal
            .get(&pane)
            .copied()
            .or(Some(TerminalId(pane.0)))
    }

    fn record_foreground_process(&mut self, terminal: TerminalId, process: ForegroundProcessInfo) {
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
        self.connect()
    }

    fn connect(&mut self) -> io::Result<()> {
        let mut stream = connect_or_spawn_server(&self.socket_path)?;
        stream.set_nonblocking(false)?;
        let mut writer_stream = stream.try_clone()?;
        write_message(
            &mut writer_stream,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )?;
        validate_server_hello_with_timeout(&mut stream, SERVER_HELLO_TIMEOUT)?;

        let writer = Arc::new(Mutex::new(writer_stream));
        let (sender, receiver) = mpsc::channel();
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
        Ok(())
    }

    fn send(&mut self, message: ClientMessage) -> io::Result<()> {
        self.ensure_connected()?;
        match self.write(&message) {
            Ok(()) => Ok(()),
            Err(error) if is_disconnected_error(&error) => {
                self.connection = None;
                self.ensure_connected()?;
                self.write(&message)
            }
            Err(error) => Err(error),
        }
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

fn validate_server_hello(reader: &mut impl io::Read) -> io::Result<()> {
    match read_message::<ServerMessage>(reader)? {
        ServerMessage::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION => Ok(()),
        ServerMessage::Hello { protocol_version } => Err(io::Error::new(
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

fn session_for_terminal(terminal: TerminalId) -> SessionId {
    SessionId(terminal.0)
}

fn pane_for_terminal(terminal: TerminalId) -> PaneId {
    PaneId(terminal.0)
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
        LaunchSpec::Shell => format!("shell {}", spawn.terminal.0),
        LaunchSpec::Command(command) => command.clone(),
    }
}

fn shell_command_args(command: String) -> Vec<String> {
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

    #[test]
    fn pty_spawn_uses_default_size() {
        let spawn = PtySpawn::shell(TerminalId(7), None, BTreeMap::new());

        assert_eq!(spawn.terminal, TerminalId(7));
        assert_eq!(spawn.args, Vec::<String>::new());
        assert_eq!(spawn.size, PtyDimensions { rows: 24, cols: 80 });
        assert!(!spawn.program.is_empty());
    }

    #[test]
    fn pty_spawn_command_line_runs_through_shell() {
        let spawn = PtySpawn::command_line(
            TerminalId(7),
            "cargo test".to_string(),
            None,
            BTreeMap::new(),
        );

        assert_eq!(spawn.terminal, TerminalId(7));
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
        let terminal = TerminalId(9);
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
        let terminal = TerminalId(9);

        runtime
            .resize(terminal, PtyDimensions { rows: 5, cols: 12 })
            .expect("resize parser");

        assert_eq!(runtime.parser(terminal).unwrap().screen().size(), (5, 12));
    }

    #[test]
    fn send_paste_wraps_when_parser_reports_bracketed_paste() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = TerminalId(7);
        let pane = PaneId(7);
        let mut runtime = PtyRuntime {
            socket_path: unique_socket_path(),
            connection: Some(ServerConnection {
                writer: Arc::new(Mutex::new(client_stream)),
                receiver,
            }),
            terminal_to_pane: HashMap::from([(terminal, pane)]),
            pane_to_terminal: HashMap::from([(pane, terminal)]),
            parsers: HashMap::new(),
            responders: HashMap::new(),
            terminals_with_output: HashSet::new(),
            terminal_exit_statuses: HashMap::new(),
            foreground_processes: HashMap::new(),
            command_trackers: HashMap::new(),
            pending_events: Vec::new(),
        };
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"\x1b[?2004h");

        assert!(runtime.send_paste(terminal, "one\ntwo").expect("paste"));

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
    fn pty_stop_sends_stop_message_and_clears_local_attachment() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = TerminalId(7);
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);

        assert!(runtime.stop(terminal).expect("stop terminal"));

        let message: ClientMessage = read_message(&mut server_stream).expect("read stop message");
        assert_eq!(message, ClientMessage::Stop { pane });
        assert!(!runtime.is_running(terminal));
        assert!(!runtime.pane_to_terminal.contains_key(&pane));
    }

    #[test]
    fn input_returns_scrolled_parser_to_bottom() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = TerminalId(7);
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree");
        assert!(runtime.scroll_up(terminal, 1).expect("scroll up"));
        assert!(runtime.parser(terminal).unwrap().screen().scrollback() > 0);

        assert!(runtime.send_input(terminal, b"x").expect("send input"));

        assert_eq!(runtime.parser(terminal).unwrap().screen().scrollback(), 0);
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
    fn parser_scrolls_beyond_visible_screen_height() {
        let mut runtime = PtyRuntime::new_offline();
        let terminal = TerminalId(7);
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
        let terminal = TerminalId(7);
        let pane = PaneId(7);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree");

        assert!(runtime.scroll_up(terminal, 1).expect("scroll up"));
        assert!(runtime.scroll_down(terminal, 1).expect("scroll down"));
        assert!(!runtime.scroll_to_top(TerminalId(99)).expect("missing"));
        assert!(runtime.send_paste(terminal, "one\ntwo").expect("paste"));

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
        let terminal = TerminalId(7);
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
            parsers: HashMap::new(),
            responders: HashMap::new(),
            terminals_with_output: HashSet::new(),
            terminal_exit_statuses: HashMap::new(),
            foreground_processes: HashMap::new(),
            command_trackers: HashMap::new(),
            pending_events: Vec::new(),
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
        let terminal = TerminalId(9);
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);

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
        let terminal = TerminalId(9);
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);

        assert!(runtime
            .send_input(terminal, b"cargo test")
            .expect("send command"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read command input");
        assert_eq!(runtime.terminal_last_command(terminal), None);

        assert!(runtime.send_input(terminal, b"\r").expect("send enter"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read enter input");

        assert_eq!(runtime.terminal_last_command(terminal), Some("cargo test"));
    }

    #[test]
    fn terminal_last_command_ignores_fullscreen_app_input() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (_sender, receiver) = mpsc::channel();
        let terminal = TerminalId(9);
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });

        assert!(runtime.send_input(terminal, b"nvim\r").expect("send nvim"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read nvim input");
        assert_eq!(runtime.terminal_last_command(terminal), Some("nvim"));

        runtime.process_terminal_output(terminal, b"\x1b[?1049h");
        assert!(runtime
            .send_input(terminal, b"asdasdq\r")
            .expect("send editor input"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read editor input");

        assert_eq!(runtime.terminal_last_command(terminal), Some("nvim"));
    }

    #[test]
    fn terminal_last_command_uses_foreground_process_not_child_input() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = TerminalId(9);
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);

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
        assert_eq!(runtime.terminal_last_command(terminal), Some("python"));

        assert!(runtime
            .send_input(terminal, b"print('typed text')\r")
            .expect("send child input"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read child input");
        assert_eq!(runtime.terminal_last_command(terminal), Some("python"));

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
            .send_input(terminal, b"cargo test\r")
            .expect("send shell input"));
        let _: ClientMessage = read_message(&mut server_stream).expect("read shell input");
        assert_eq!(runtime.terminal_last_command(terminal), Some("cargo test"));
    }

    #[test]
    fn live_output_answers_primary_device_attributes_query() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = TerminalId(9);
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });

        sender
            .send(ServerMessage::PtyOutput {
                pane,
                bytes: b"\x1b[c".to_vec(),
            })
            .expect("send terminal query");

        let events = runtime.drain_events();
        let message: ClientMessage = read_message(&mut server_stream).expect("read DA response");

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
                bytes: PRIMARY_DEVICE_ATTRIBUTES_RESPONSE.to_vec(),
            }
        );
    }

    #[test]
    fn output_event_feeds_matching_parser() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = TerminalId(9);
        let pane = PaneId(9);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });

        sender
            .send(ServerMessage::PtyOutput {
                pane,
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
    fn disconnected_receiver_clears_registries_for_reconnect_but_keeps_parser() {
        let (client_stream, _server_stream) = UnixStream::pair().expect("create socket pair");
        let (sender, receiver) = mpsc::channel();
        let terminal = TerminalId(10);
        let pane = PaneId(10);
        let mut runtime = test_runtime(client_stream, receiver, terminal, pane);
        runtime.ensure_parser(terminal, PtyDimensions { rows: 2, cols: 8 });
        drop(sender);

        assert!(runtime.drain_events().is_empty());

        assert!(runtime.connection.is_none());
        assert!(!runtime.is_running(terminal));
        assert!(runtime.pane_to_terminal.is_empty());
        assert!(runtime.parser(terminal).is_some());
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
        terminal: TerminalId,
        pane: PaneId,
    ) -> PtyRuntime {
        PtyRuntime {
            socket_path: unique_socket_path(),
            connection: Some(ServerConnection {
                writer: Arc::new(Mutex::new(client_stream)),
                receiver,
            }),
            terminal_to_pane: HashMap::from([(terminal, pane)]),
            pane_to_terminal: HashMap::from([(pane, terminal)]),
            parsers: HashMap::new(),
            responders: HashMap::new(),
            terminals_with_output: HashSet::new(),
            terminal_exit_statuses: HashMap::new(),
            foreground_processes: HashMap::new(),
            command_trackers: HashMap::new(),
            pending_events: Vec::new(),
        }
    }

    fn unique_socket_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-pty-test-{unique}.sock"))
    }
}
