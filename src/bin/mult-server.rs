use std::{
    collections::BTreeMap,
    env, fs, io,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
};

use mult::app::TerminalBuffer;
use mult_protocol::{
    read_message, write_message, ClientMessage, ExitInfo, LaunchSpec, PaneId, PaneInfo,
    ScreenSnapshot, ServerMessage, SessionId, SessionInfo, DEFAULT_SOCKET_NAME, PROTOCOL_VERSION,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

type ClientId = u64;
type SharedServer = Arc<Mutex<ServerState>>;
type SharedPane = Arc<Mutex<PaneState>>;
type SharedPtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;
type SharedMasterPty = Arc<Mutex<Box<dyn MasterPty + Send>>>;
type ClientSender = mpsc::Sender<ServerMessage>;

#[derive(Clone)]
struct ClientHandle {
    id: ClientId,
    sender: ClientSender,
}

struct ServerState {
    sessions: BTreeMap<SessionId, SharedPane>,
    next_session_id: u64,
    next_client_id: ClientId,
}

struct PaneState {
    session: SessionId,
    pane: PaneId,
    name: String,
    title: String,
    terminal: TerminalBuffer,
    master: SharedMasterPty,
    writer: SharedPtyWriter,
    _child: Box<dyn Child + Send + Sync>,
    clients: Vec<ClientHandle>,
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

fn main() -> io::Result<()> {
    let socket_path = socket_path();
    bind_socket_path(&socket_path)?;
    let server = Arc::new(Mutex::new(ServerState::default()));
    let listener = UnixListener::bind(&socket_path)?;
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
            next_session_id: 1,
            next_client_id: 1,
        }
    }
}

impl ServerState {
    fn allocate_client_id(&mut self) -> ClientId {
        let id = self.next_client_id;
        self.next_client_id += 1;
        id
    }

    fn allocate_session_id(&mut self) -> SessionId {
        while self.sessions.contains_key(&SessionId(self.next_session_id)) {
            self.next_session_id += 1;
        }
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;
        id
    }

    fn session_infos(&self) -> Vec<SessionInfo> {
        self.sessions
            .values()
            .filter_map(|pane| pane.lock().ok().map(|pane| pane.session_info()))
            .collect()
    }

    fn pane_by_id(&self, pane: PaneId) -> Option<SharedPane> {
        self.sessions.get(&SessionId(pane.0)).cloned().or_else(|| {
            self.sessions.values().find_map(|candidate| {
                let matches = candidate
                    .lock()
                    .ok()
                    .is_some_and(|candidate| candidate.pane == pane);
                matches.then(|| Arc::clone(candidate))
            })
        })
    }

    fn remove_client(&mut self, client_id: ClientId) {
        for pane in self.sessions.values() {
            if let Ok(mut pane) = pane.lock() {
                pane.clients.retain(|client| client.id != client_id);
            }
        }
    }
}

fn bind_socket_path(path: &PathBuf) -> io::Result<()> {
    if path.exists() {
        match UnixStream::connect(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("server already listening at {}", path.display()),
                ));
            }
            Err(_) => fs::remove_file(path)?,
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
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

fn spawn_pane(session: SessionId, spec: PaneSpawnSpec) -> io::Result<SharedPane> {
    let rows = spec.rows.max(1);
    let cols = spec.cols.max(1);
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
    for (key, value) in spec.env {
        command.env(key, value);
    }

    let child = pair.slave.spawn_command(command).map_err(error_to_io)?;
    let reader = pair.master.try_clone_reader().map_err(error_to_io)?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(error_to_io)?));
    let master = Arc::new(Mutex::new(pair.master));

    let mut terminal = TerminalBuffer::default();
    terminal.resize(rows, cols);
    let title = pane_title(&shell, &spec.launch);

    let pane = Arc::new(Mutex::new(PaneState {
        session,
        pane: PaneId(session.0),
        name: spec.name,
        title,
        terminal,
        master,
        writer,
        _child: child,
        clients: Vec::new(),
    }));

    spawn_reader(reader, Arc::clone(&pane));
    Ok(pane)
}

fn handle_client(stream: UnixStream, server: SharedServer) -> io::Result<()> {
    let (sender, receiver) = mpsc::channel();
    let client_id = server.lock().map_err(lock_error)?.allocate_client_id();
    let client = ClientHandle {
        id: client_id,
        sender: sender.clone(),
    };

    let mut writer_stream = stream.try_clone()?;
    let _writer = thread::spawn(move || {
        for message in receiver {
            if write_message(&mut writer_stream, &message).is_err() {
                break;
            }
        }
    });

    let result = handle_client_messages(stream, &server, client);
    if let Ok(mut server) = server.lock() {
        server.remove_client(client_id);
    }
    drop(sender);
    result
}

fn handle_client_messages(
    mut stream: UnixStream,
    server: &SharedServer,
    client: ClientHandle,
) -> io::Result<()> {
    loop {
        let message = match read_message::<ClientMessage>(&mut stream) {
            Ok(message) => message,
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
            Err(error) => return Err(error),
        };

        match message {
            ClientMessage::Hello { .. } => {
                let _ = client.sender.send(ServerMessage::Hello {
                    protocol_version: PROTOCOL_VERSION,
                });
            }
            ClientMessage::ListSessions => {
                let sessions = server.lock().map_err(lock_error)?.session_infos();
                let _ = client.sender.send(ServerMessage::Sessions(sessions));
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
                let pane = create_session(
                    server,
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
                )?;
                let info = pane.lock().map_err(lock_error)?.session_info();
                let _ = client.sender.send(ServerMessage::Sessions(vec![info]));
            }
            ClientMessage::Attach {
                session,
                rows,
                cols,
            } => {
                let pane = {
                    let server = server.lock().map_err(lock_error)?;
                    server.sessions.get(&session).cloned()
                };
                let Some(pane) = pane else {
                    let _ = client.sender.send(ServerMessage::Error {
                        message: format!("unknown session {}", session.0),
                    });
                    continue;
                };

                let (pane_info, snapshot) = {
                    let mut pane = pane.lock().map_err(lock_error)?;
                    if pane.clients.iter().any(|existing| existing.id != client.id) {
                        let _ = client.sender.send(ServerMessage::Error {
                            message: format!("session {} is already attached", session.0),
                        });
                        continue;
                    }
                    pane.resize(rows, cols)?;
                    if !pane.clients.iter().any(|existing| existing.id == client.id) {
                        pane.clients.push(client.clone());
                    }
                    (pane.pane_info(), pane.terminal.snapshot())
                };

                let _ = client.sender.send(ServerMessage::Attached {
                    session,
                    panes: vec![pane_info],
                });
                let _ = client.sender.send(ServerMessage::Snapshot {
                    pane: PaneId(session.0),
                    snapshot,
                });
            }
            ClientMessage::Input { pane, bytes } => {
                let pane = { server.lock().map_err(lock_error)?.pane_by_id(pane) };
                if let Some(pane) = pane {
                    let writer = { Arc::clone(&pane.lock().map_err(lock_error)?.writer) };
                    write_pty_input(&writer, &bytes)?;
                }
            }
            ClientMessage::Resize { pane, rows, cols } => {
                let pane = { server.lock().map_err(lock_error)?.pane_by_id(pane) };
                if let Some(pane) = pane {
                    let (pane_id, snapshot) = {
                        let mut pane = pane.lock().map_err(lock_error)?;
                        pane.resize(rows, cols)?;
                        (pane.pane, pane.terminal.snapshot())
                    };
                    let _ = client.sender.send(ServerMessage::Snapshot {
                        pane: pane_id,
                        snapshot,
                    });
                }
            }
            ClientMessage::Detach => break,
        }
    }

    Ok(())
}

fn create_session(server: &SharedServer, spec: SessionCreateSpec) -> io::Result<SharedPane> {
    let session = {
        let mut server = server.lock().map_err(lock_error)?;
        spec.requested_id
            .unwrap_or_else(|| server.allocate_session_id())
    };

    if let Some(existing) = server
        .lock()
        .map_err(lock_error)?
        .sessions
        .get(&session)
        .cloned()
    {
        return Ok(existing);
    }

    let pane = spawn_pane(session, spec.pane)?;
    server
        .lock()
        .map_err(lock_error)?
        .sessions
        .insert(session, Arc::clone(&pane));
    Ok(pane)
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, pane: SharedPane) {
    thread::spawn(move || {
        let mut buffer = [0; 8192];
        let mut query_tail = Vec::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let writer = match pane.lock() {
                        Ok(pane) => Arc::clone(&pane.writer),
                        Err(_) => break,
                    };
                    write_terminal_query_responses(&buffer[..n], &mut query_tail, &writer);

                    let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                    let (pane_id, snapshot, clients) = match pane.lock() {
                        Ok(mut pane) => {
                            pane.terminal.append(&text);
                            (pane.pane, pane.terminal.snapshot(), pane.clients.clone())
                        }
                        Err(_) => break,
                    };
                    broadcast_snapshot(pane_id, snapshot, clients);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    eprintln!("failed to read PTY output: {error}");
                    break;
                }
            }
        }

        let (pane_id, clients) = match pane.lock() {
            Ok(pane) => (pane.pane, pane.clients.clone()),
            Err(_) => (PaneId(0), Vec::new()),
        };
        for client in clients {
            let _ = client.sender.send(ServerMessage::PaneExited {
                pane: pane_id,
                exit: ExitInfo {
                    code: 0,
                    signal: None,
                },
            });
        }
    });
}

fn broadcast_snapshot(pane: PaneId, snapshot: ScreenSnapshot, clients: Vec<ClientHandle>) {
    for client in clients {
        let _ = client.sender.send(ServerMessage::Update {
            pane,
            snapshot: snapshot.clone(),
        });
    }
}

impl PaneState {
    fn session_info(&self) -> SessionInfo {
        SessionInfo {
            id: self.session,
            name: self.name.clone(),
            pane: self.pane,
            attached: !self.clients.is_empty(),
        }
    }

    fn pane_info(&self) -> PaneInfo {
        let snapshot = self.terminal.snapshot();
        PaneInfo {
            id: self.pane,
            title: self.title.clone(),
            rows: snapshot.rows,
            cols: snapshot.cols,
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        self.terminal.resize(rows, cols);
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
}

fn write_pty_input(writer: &SharedPtyWriter, bytes: &[u8]) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("PTY writer lock poisoned"))?;
    writer.write_all(bytes)?;
    writer.flush()
}

const TERMINAL_QUERY_TAIL_BYTES: usize = 16;
const PRIMARY_DEVICE_ATTRIBUTES_RESPONSE: &[u8] = b"\x1b[?1;2c";

fn write_terminal_query_responses(
    bytes: &[u8],
    query_tail: &mut Vec<u8>,
    writer: &SharedPtyWriter,
) {
    let already_seen = query_tail.len();
    let mut scan = Vec::with_capacity(already_seen + bytes.len());
    scan.extend_from_slice(query_tail);
    scan.extend_from_slice(bytes);

    let response_count = primary_device_attribute_query_count(&scan, already_seen);
    if response_count > 0 {
        if let Ok(mut writer) = writer.lock() {
            for _ in 0..response_count {
                let _ = writer.write_all(PRIMARY_DEVICE_ATTRIBUTES_RESPONSE);
            }
            let _ = writer.flush();
        }
    }

    query_tail.clear();
    let keep = scan.len().min(TERMINAL_QUERY_TAIL_BYTES);
    query_tail.extend_from_slice(&scan[scan.len().saturating_sub(keep)..]);
}

fn primary_device_attribute_query_count(bytes: &[u8], already_seen: usize) -> usize {
    let mut count = 0;
    let mut index = 0;
    while index < bytes.len() {
        let Some((end, is_query)) = primary_device_attribute_query_at(bytes, index) else {
            index += 1;
            continue;
        };

        if is_query && end > already_seen {
            count += 1;
        }
        index = end;
    }

    count
}

fn primary_device_attribute_query_at(bytes: &[u8], index: usize) -> Option<(usize, bool)> {
    if bytes.get(index) != Some(&0x1b) {
        return None;
    }

    if bytes.get(index + 1) == Some(&b'Z') {
        return Some((index + 2, true));
    }

    if bytes.get(index + 1) != Some(&b'[') {
        return None;
    }

    let mut final_index = index + 2;
    while let Some(byte) = bytes.get(final_index) {
        if (0x40..=0x7e).contains(byte) {
            break;
        }
        final_index += 1;
    }

    let final_byte = bytes.get(final_index)?;
    if *final_byte != b'c' {
        return Some((final_index + 1, false));
    }

    let params = &bytes[index + 2..final_index];
    let is_primary_query = params.is_empty() || params == b"0";
    Some((final_index + 1, is_primary_query))
}

fn shell_command_args(command: String) -> Vec<String> {
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
