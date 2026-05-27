use std::{
    collections::BTreeMap,
    env, fs, io,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};

use mult_protocol::{
    bounded_screen_dimensions, default_socket_path, read_message, write_message, ClientMessage,
    ExitInfo, ForegroundProcessInfo, LaunchSpec, PaneId, PaneInfo, ServerMessage, SessionId,
    SessionInfo, MAX_MESSAGE_BYTES, PROTOCOL_VERSION,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};

type ClientId = u64;
type SharedServer = Arc<Mutex<ServerState>>;
type SharedPane = Arc<Mutex<PaneState>>;
type SharedPtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;
type SharedMasterPty = Arc<Mutex<Box<dyn MasterPty + Send>>>;
type ClientSender = mpsc::Sender<ServerMessage>;

const CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const RAW_HISTORY_MAX_BYTES: usize = MAX_MESSAGE_BYTES * 2;
const RAW_HISTORY_CHUNK_BYTES: usize = 64 * 1024;

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
    rows: u16,
    cols: u16,
    raw_history: Vec<u8>,
    master: SharedMasterPty,
    writer: SharedPtyWriter,
    child_pid: Option<u32>,
    foreground_process: ForegroundProcessInfo,
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
    ignore_hangup_signal()?;
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
    let child_pid = child.process_id();
    let reader = pair.master.try_clone_reader().map_err(error_to_io)?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(error_to_io)?));
    let master = Arc::new(Mutex::new(pair.master));
    let title = pane_title(&shell, &spec.launch);

    let pane = Arc::new(Mutex::new(PaneState {
        session,
        pane: PaneId(session.0),
        name: spec.name,
        title,
        rows,
        cols,
        raw_history: Vec::new(),
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

                let (pane_info, pane_id, history, foreground_process) = {
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
                    let foreground_process = pane.refresh_foreground_process();
                    (
                        pane.pane_info(),
                        pane.pane,
                        pane.raw_history.clone(),
                        foreground_process,
                    )
                };

                let _ = client.sender.send(ServerMessage::Attached {
                    session,
                    panes: vec![pane_info],
                });
                let _ = client.sender.send(ServerMessage::ForegroundProcess {
                    pane: pane_id,
                    process: foreground_process,
                });
                send_pty_scrollback(&client, pane_id, &history);
            }
            ClientMessage::Input { pane, bytes } => {
                let pane = { server.lock().map_err(lock_error)?.pane_by_id(pane) };
                if let Some(pane) = pane {
                    let writer = { Arc::clone(&pane.lock().map_err(lock_error)?.writer) };
                    write_pty_input(&writer, &bytes)?;
                    if input_may_change_foreground(&bytes) {
                        schedule_foreground_process_poll(pane);
                    }
                }
            }
            ClientMessage::Paste { pane, text } => {
                let pane = { server.lock().map_err(lock_error)?.pane_by_id(pane) };
                if let Some(pane) = pane {
                    let writer = { Arc::clone(&pane.lock().map_err(lock_error)?.writer) };
                    write_pty_input(&writer, text.as_bytes())?;
                    if input_may_change_foreground(text.as_bytes()) {
                        schedule_foreground_process_poll(pane);
                    }
                }
            }
            ClientMessage::Scroll { .. }
            | ClientMessage::ScrollToTop { .. }
            | ClientMessage::ScrollToBottom { .. } => {}
            ClientMessage::Resize { pane, rows, cols } => {
                let pane = { server.lock().map_err(lock_error)?.pane_by_id(pane) };
                if let Some(pane) = pane {
                    pane.lock().map_err(lock_error)?.resize(rows, cols)?;
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
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = buffer[..n].to_vec();
                    let (pane_id, clients) = match pane.lock() {
                        Ok(mut pane) => {
                            pane.append_raw_history(&bytes);
                            (pane.pane, pane.clients.clone())
                        }
                        Err(_) => break,
                    };
                    broadcast_pty_output(pane_id, bytes, clients);
                    broadcast_foreground_process_if_changed(&pane);
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

fn send_pty_scrollback(client: &ClientHandle, pane: PaneId, bytes: &[u8]) {
    if bytes.is_empty() {
        let _ = client.sender.send(ServerMessage::PtyScrollback {
            pane,
            bytes: Vec::new(),
        });
        return;
    }

    for chunk in bytes.chunks(RAW_HISTORY_CHUNK_BYTES) {
        let _ = client.sender.send(ServerMessage::PtyScrollback {
            pane,
            bytes: chunk.to_vec(),
        });
    }
}

fn broadcast_foreground_process_if_changed(pane: &SharedPane) {
    let Some((pane_id, process, clients)) = foreground_process_update(pane) else {
        return;
    };

    for client in clients {
        let _ = client.sender.send(ServerMessage::ForegroundProcess {
            pane: pane_id,
            process: process.clone(),
        });
    }
}

fn foreground_process_update(
    pane: &SharedPane,
) -> Option<(PaneId, ForegroundProcessInfo, Vec<ClientHandle>)> {
    let mut pane = pane.lock().ok()?;
    let process = pane.refresh_foreground_process_if_changed()?;
    Some((pane.pane, process, pane.clients.clone()))
}

fn schedule_foreground_process_poll(pane: SharedPane) {
    thread::spawn(move || {
        for delay in [
            Duration::from_millis(25),
            Duration::from_millis(100),
            Duration::from_millis(500),
        ] {
            thread::sleep(delay);
            broadcast_foreground_process_if_changed(&pane);
        }
    });
}

fn input_may_change_foreground(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|byte| matches!(*byte, b'\r' | b'\n' | 0x03 | 0x1a))
}

fn broadcast_exit(pane: PaneId, exit: ExitInfo, clients: Vec<ClientHandle>) {
    for client in clients {
        let _ = client.sender.send(ServerMessage::PaneExited {
            pane,
            exit: exit.clone(),
        });
    }
}

fn broadcast_pty_output(pane: PaneId, bytes: Vec<u8>, clients: Vec<ClientHandle>) {
    for client in clients {
        let _ = client.sender.send(ServerMessage::PtyOutput {
            pane,
            bytes: bytes.clone(),
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
        PaneInfo {
            id: self.pane,
            title: self.title.clone(),
            rows: self.rows,
            cols: self.cols,
        }
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
        let command = foreground_pid.and_then(command_line_for_pid);
        ForegroundProcessInfo {
            root_pid: self.child_pid,
            foreground_pid,
            command,
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

    fn stop(&mut self) -> io::Result<()> {
        if let Some(child) = self.child.as_mut() {
            child.kill()?;
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        Ok(())
    }

    fn append_raw_history(&mut self, bytes: &[u8]) {
        self.raw_history.extend_from_slice(bytes);
        let overflow = self.raw_history.len().saturating_sub(RAW_HISTORY_MAX_BYTES);
        if overflow > 0 {
            self.raw_history.drain(..overflow);
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

fn write_pty_input(writer: &SharedPtyWriter, bytes: &[u8]) -> io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("PTY writer lock poisoned"))?;
    writer.write_all(bytes)?;
    writer.flush()
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
    fn raw_history_is_capped() {
        let mut pane = test_pane_state();
        pane.append_raw_history(&vec![b'a'; RAW_HISTORY_MAX_BYTES + 10]);

        assert_eq!(pane.raw_history.len(), RAW_HISTORY_MAX_BYTES);
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
            pane: PaneId(1),
            name: "test".to_string(),
            title: "test".to_string(),
            rows: 1,
            cols: 1,
            raw_history: Vec::new(),
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(Box::new(io::sink()))),
            child_pid: None,
            foreground_process: ForegroundProcessInfo {
                root_pid: None,
                foreground_pid: None,
                command: None,
            },
            child: None,
            clients: Vec::new(),
        }
    }

    fn unique_socket_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-server-test-{unique}.sock"))
    }
}
