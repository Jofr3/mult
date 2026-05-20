use std::{
    collections::{BTreeMap, HashMap},
    env, io,
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
    read_message, write_message, ClientMessage, LaunchSpec, PaneId, ScreenSnapshot, ScreenUpdate,
    ServerMessage, SessionId, DEFAULT_SOCKET_NAME, PROTOCOL_VERSION,
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
        let Some(pane) = self.terminal_to_pane.remove(&terminal) else {
            return Ok(false);
        };
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
        let stream = connect_or_spawn_server()?;
        stream.set_nonblocking(false)?;
        let mut writer_stream = stream.try_clone()?;
        write_message(
            &mut writer_stream,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )?;

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

fn connect_or_spawn_server() -> io::Result<UnixStream> {
    let path = socket_path();
    match UnixStream::connect(&path) {
        Ok(stream) => Ok(stream),
        Err(error) if should_autospawn_server(&error) => {
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

fn should_autospawn_server(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    ) && autospawn_enabled()
        && server_executable().is_some()
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

fn socket_path() -> PathBuf {
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join(DEFAULT_SOCKET_NAME);
    }

    let user = env::var("UID")
        .or_else(|_| env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from(format!("/tmp/mult-{user}.sock"))
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
}
