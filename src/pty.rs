use std::{
    collections::{BTreeMap, HashMap},
    env, fs, io,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use mult_protocol::{
    default_socket_path, read_message, write_message, ClientMessage, LaunchSpec, PaneId,
    ScreenSnapshot, ScreenUpdate, ServerMessage, SessionId, PROTOCOL_VERSION,
};

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
    Snapshot {
        terminal: TerminalId,
        snapshot: ScreenSnapshot,
    },
    Update {
        terminal: TerminalId,
        update: ScreenUpdate,
    },
    Output {
        terminal: TerminalId,
        text: String,
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
    connection: Option<ServerConnection>,
    terminal_to_pane: HashMap<TerminalId, PaneId>,
    pane_to_terminal: HashMap<PaneId, TerminalId>,
    pending_events: Vec<PtyEvent>,
}

const SERVER_HELLO_TIMEOUT: Duration = Duration::from_secs(2);

struct ServerConnection {
    writer: Arc<Mutex<UnixStream>>,
    receiver: Receiver<ServerMessage>,
}

impl Default for PtyRuntime {
    fn default() -> Self {
        let mut runtime = Self {
            connection: None,
            terminal_to_pane: HashMap::new(),
            pane_to_terminal: HashMap::new(),
            pending_events: Vec::new(),
        };
        if let Err(error) = runtime.connect() {
            runtime.pending_events.push(PtyEvent::Error {
                terminal: TerminalId(0),
                message: format!("failed to connect to mult-server: {error}"),
            });
        }
        runtime
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

    pub fn start(&mut self, spawn: PtySpawn) -> io::Result<()> {
        if self.is_running(spawn.terminal) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal already has a server attachment",
            ));
        }

        self.ensure_connected()?;
        let session = session_for_terminal(spawn.terminal);
        let pane = pane_for_terminal(spawn.terminal);
        let launch = launch_spec(&spawn);
        let name = session_name(&spawn, &launch);
        self.send(ClientMessage::CreateSession {
            requested_id: Some(session),
            name,
            cwd: spawn.cwd.clone(),
            env: spawn.env.clone(),
            launch,
            rows: spawn.size.rows,
            cols: spawn.size.cols,
        })?;
        self.send(ClientMessage::Attach {
            session,
            rows: spawn.size.rows,
            cols: spawn.size.cols,
        })?;
        self.terminal_to_pane.insert(spawn.terminal, pane);
        self.pane_to_terminal.insert(pane, spawn.terminal);
        Ok(())
    }

    pub fn stop(&mut self, terminal: TerminalId) -> io::Result<bool> {
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
            return Ok(false);
        };

        self.send(ClientMessage::Stop { pane })?;
        self.terminal_to_pane.remove(&terminal);
        self.pane_to_terminal.remove(&pane);
        Ok(true)
    }

    pub fn send_input(&mut self, terminal: TerminalId, input: &[u8]) -> io::Result<bool> {
        let Some(pane) = self.terminal_to_pane.get(&terminal).copied() else {
            return Ok(false);
        };
        self.send(ClientMessage::Input {
            pane,
            bytes: input.to_vec(),
        })?;
        Ok(true)
    }

    pub fn resize(&mut self, terminal: TerminalId, size: PtyDimensions) -> io::Result<()> {
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
        let mut disconnected = false;

        if let Some(connection) = &self.connection {
            while let Ok(message) = connection.receiver.try_recv() {
                match message {
                    ServerMessage::Hello { .. }
                    | ServerMessage::Sessions(_)
                    | ServerMessage::Attached { .. } => {}
                    ServerMessage::Snapshot { pane, snapshot } => {
                        if let Some(terminal) = self.pane_to_terminal.get(&pane).copied() {
                            events.push(PtyEvent::Snapshot { terminal, snapshot });
                        }
                    }
                    ServerMessage::Update { pane, update } => {
                        if let Some(terminal) = self.pane_to_terminal.get(&pane).copied() {
                            events.push(PtyEvent::Update { terminal, update });
                        }
                    }
                    ServerMessage::PaneExited { pane, exit } => {
                        if let Some(terminal) = self.pane_to_terminal.remove(&pane) {
                            self.terminal_to_pane.remove(&terminal);
                            events.push(PtyEvent::Exited {
                                terminal,
                                status: PtyExit {
                                    code: exit.code,
                                    signal: exit.signal,
                                },
                            });
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
        } else {
            disconnected = true;
        }

        if disconnected {
            self.terminal_to_pane.clear();
            self.pane_to_terminal.clear();
        }

        events
    }

    fn ensure_connected(&mut self) -> io::Result<()> {
        if self.connection.is_some() {
            return Ok(());
        }
        self.connect()
    }

    fn connect(&mut self) -> io::Result<()> {
        let mut stream = connect_or_spawn_server()?;
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

fn connect_or_spawn_server() -> io::Result<UnixStream> {
    let path = default_socket_path();
    match UnixStream::connect(&path) {
        Ok(stream) => Ok(stream),
        Err(error) if should_autospawn_server(&error, &path) => {
            spawn_server()?;
            wait_for_server(&path).map_err(|wait_error| {
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

fn spawn_server() -> io::Result<()> {
    let server = server_executable().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate mult-server next to the mult executable; run `mult-server` manually",
        )
    })?;

    Command::new(server)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
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
        let mut runtime = PtyRuntime {
            connection: Some(ServerConnection {
                writer: Arc::new(Mutex::new(client_stream)),
                receiver,
            }),
            terminal_to_pane: HashMap::from([(terminal, pane)]),
            pane_to_terminal: HashMap::from([(pane, terminal)]),
            pending_events: Vec::new(),
        };

        assert!(runtime.stop(terminal).expect("stop terminal"));

        let message: ClientMessage = read_message(&mut server_stream).expect("read stop message");
        assert_eq!(message, ClientMessage::Stop { pane });
        assert!(!runtime.is_running(terminal));
        assert!(!runtime.pane_to_terminal.contains_key(&pane));
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
            connection: Some(ServerConnection { writer, receiver }),
            terminal_to_pane: HashMap::from([(terminal, pane)]),
            pane_to_terminal: HashMap::from([(pane, terminal)]),
            pending_events: Vec::new(),
        };

        let error = runtime.stop(terminal).expect_err("stop should fail");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(runtime.is_running(terminal));
        assert_eq!(runtime.pane_to_terminal.get(&pane), Some(&terminal));
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

    fn unique_socket_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-pty-test-{unique}.sock"))
    }
}
