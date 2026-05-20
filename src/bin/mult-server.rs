use std::{
    collections::BTreeMap,
    env, fs, io,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};

use mult::app::TerminalBuffer;
use mult_protocol::{
    bounded_screen_dimensions, default_socket_path, read_message, write_message, ClientMessage,
    ExitInfo, LaunchSpec, PaneId, PaneInfo, ScreenSnapshot, ScreenUpdate, ServerMessage, SessionId,
    SessionInfo, PROTOCOL_VERSION,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};

type ClientId = u64;
type SharedServer = Arc<Mutex<ServerState>>;
type SharedPane = Arc<Mutex<PaneState>>;
type SharedPtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;
type SharedMasterPty = Arc<Mutex<Box<dyn MasterPty + Send>>>;
type ClientSender = mpsc::Sender<ServerMessage>;

const CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(2);

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
    last_snapshot: Option<ScreenSnapshot>,
    master: SharedMasterPty,
    writer: SharedPtyWriter,
    child: Option<Box<dyn Child + Send + Sync>>,
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

struct SpawnedPane {
    pane: SharedPane,
    reader: Box<dyn Read + Send>,
}

fn main() -> io::Result<()> {
    let socket_path = default_socket_path();
    bind_socket_path(&socket_path)?;
    let server = Arc::new(Mutex::new(ServerState::default()));
    let listener = UnixListener::bind(&socket_path)?;
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

    fn remove_pane_by_id(&mut self, pane: PaneId) -> Option<SharedPane> {
        if let Some(removed) = self.sessions.remove(&SessionId(pane.0)) {
            return Some(removed);
        }

        let session = self.sessions.iter().find_map(|(session, candidate)| {
            let matches = candidate
                .lock()
                .ok()
                .is_some_and(|candidate| candidate.pane == pane);
            matches.then_some(*session)
        })?;
        self.sessions.remove(&session)
    }

    fn remove_session_if_same(&mut self, session: SessionId, pane: &SharedPane) -> bool {
        let matches = self
            .sessions
            .get(&session)
            .is_some_and(|existing| Arc::ptr_eq(existing, pane));
        if matches {
            self.sessions.remove(&session);
        }
        matches
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
        fs::create_dir_all(parent)?;
    }
    Ok(())
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

fn spawn_pane(session: SessionId, spec: PaneSpawnSpec) -> io::Result<SpawnedPane> {
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
    for (key, value) in spec.env {
        command.env(key, value);
    }

    let child = pair.slave.spawn_command(command).map_err(error_to_io)?;
    let reader = pair.master.try_clone_reader().map_err(error_to_io)?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(error_to_io)?));
    let master = Arc::new(Mutex::new(pair.master));

    let mut terminal = TerminalBuffer::default();
    terminal.resize(rows, cols);
    let last_snapshot = Some(terminal.snapshot());
    let title = pane_title(&shell, &spec.launch);

    let pane = Arc::new(Mutex::new(PaneState {
        session,
        pane: PaneId(session.0),
        name: spec.name,
        title,
        terminal,
        last_snapshot,
        master,
        writer,
        child: Some(child),
        clients: Vec::new(),
    }));

    Ok(SpawnedPane { pane, reader })
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
    stream.set_read_timeout(Some(CLIENT_HELLO_TIMEOUT))?;
    let Some(message) = read_client_message(&mut stream)? else {
        return Ok(());
    };
    stream.set_read_timeout(None)?;

    let ClientMessage::Hello { protocol_version } = message else {
        let _ = client.sender.send(ServerMessage::Error {
            message: "expected protocol hello before other client messages".to_string(),
        });
        return Ok(());
    };

    if !send_hello_response(&client, protocol_version) {
        return Ok(());
    }

    while let Some(message) = read_client_message(&mut stream)? {
        match message {
            ClientMessage::Hello { protocol_version } => {
                if !send_hello_response(&client, protocol_version) {
                    break;
                }
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
                    let snapshot = pane.terminal.snapshot();
                    pane.last_snapshot = Some(snapshot.clone());
                    (pane.pane_info(), snapshot)
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
                        let snapshot = pane.terminal.snapshot();
                        pane.last_snapshot = Some(snapshot.clone());
                        (pane.pane, snapshot)
                    };
                    let _ = client.sender.send(ServerMessage::Snapshot {
                        pane: pane_id,
                        snapshot,
                    });
                }
            }
            ClientMessage::Detach => break,
            ClientMessage::Stop { pane } => {
                let target = { server.lock().map_err(lock_error)?.pane_by_id(pane) };
                let Some(target) = target else {
                    continue;
                };

                let stop_result = target.lock().map_err(lock_error)?.stop();
                match stop_result {
                    Ok(()) => {
                        server.lock().map_err(lock_error)?.remove_pane_by_id(pane);
                    }
                    Err(error) => {
                        let _ = client.sender.send(ServerMessage::Error {
                            message: format!("failed to stop pane: {error}"),
                        });
                    }
                }
            }
        }
    }

    Ok(())
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

fn send_hello_response(client: &ClientHandle, protocol_version: u16) -> bool {
    if protocol_version != PROTOCOL_VERSION {
        let _ = client.sender.send(ServerMessage::Error {
            message: format!(
                "client protocol version {protocol_version} is incompatible with server version {PROTOCOL_VERSION}; restart mult clients"
            ),
        });
        return false;
    }

    let _ = client.sender.send(ServerMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
    });
    true
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

    let spawned = spawn_pane(session, spec.pane)?;
    server
        .lock()
        .map_err(lock_error)?
        .sessions
        .insert(session, Arc::clone(&spawned.pane));
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
                    let (pane_id, update, clients) = match pane.lock() {
                        Ok(mut pane) => {
                            pane.terminal.append(&text);
                            let snapshot = pane.terminal.snapshot();
                            let update = pane
                                .last_snapshot
                                .as_ref()
                                .map(|previous| ScreenUpdate::diff(previous, &snapshot))
                                .unwrap_or_else(|| ScreenUpdate::from_snapshot(&snapshot));
                            pane.last_snapshot = Some(snapshot);
                            (pane.pane, update, pane.clients.clone())
                        }
                        Err(_) => break,
                    };
                    broadcast_update(pane_id, update, clients);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    eprintln!("failed to read PTY output: {error}");
                    break;
                }
            }
        }

        let Some((session, pane_id, clients, exit)) = pane_exit(&pane) else {
            return;
        };
        if let Ok(mut server) = server.lock() {
            server.remove_session_if_same(session, &pane);
        }
        broadcast_exit(pane_id, exit, clients);
    });
}

fn pane_exit(pane: &SharedPane) -> Option<(SessionId, PaneId, Vec<ClientHandle>, ExitInfo)> {
    let (session, pane_id, clients, mut child) = {
        let mut pane = pane.lock().ok()?;
        let child = pane.child.take()?;
        (pane.session, pane.pane, pane.clients.clone(), child)
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

fn broadcast_exit(pane: PaneId, exit: ExitInfo, clients: Vec<ClientHandle>) {
    for client in clients {
        let _ = client.sender.send(ServerMessage::PaneExited {
            pane,
            exit: exit.clone(),
        });
    }
}

fn broadcast_update(pane: PaneId, update: ScreenUpdate, clients: Vec<ClientHandle>) {
    for client in clients {
        let _ = client.sender.send(ServerMessage::Update {
            pane,
            update: update.clone(),
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
        let (rows, cols) = bounded_pty_dimensions(rows, cols);
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

    fn stop(&mut self) -> io::Result<()> {
        if let Some(child) = &mut self.child {
            child.kill()
        } else {
            Ok(())
        }
    }
}

fn bounded_pty_dimensions(rows: u16, cols: u16) -> (u16, u16) {
    bounded_screen_dimensions(rows.max(1), cols.max(1))
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

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::*;

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
            },
        )
        .expect("write incompatible hello");

        let message: ServerMessage = read_message(&mut client_stream).expect("read error");
        assert!(
            matches!(message, ServerMessage::Error { message } if message.contains("incompatible"))
        );
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
        assert!(
            matches!(message, ServerMessage::Error { message } if message.contains("expected protocol hello"))
        );
        server_thread
            .join()
            .expect("server thread should not panic")
            .expect("server rejects non-hello first message");
    }

    #[test]
    fn pty_dimensions_are_bounded_for_server_allocations() {
        let (rows, cols) = bounded_pty_dimensions(u16::MAX, u16::MAX);

        assert!(usize::from(rows) * usize::from(cols) <= mult_protocol::MAX_SCREEN_CELLS);
        assert!(rows > 0);
        assert!(cols > 0);
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

    fn unique_socket_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-server-test-{unique}.sock"))
    }
}
