use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, DirBuilder},
    io,
    io::{Read, Write},
    net::Shutdown,
    os::unix::{
        fs::DirBuilderExt,
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
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
type ClientSender = mpsc::SyncSender<ServerMessage>;

const CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const RAW_HISTORY_MAX_BYTES: usize = MAX_MESSAGE_BYTES * 2;
const RAW_HISTORY_CHUNK_BYTES: usize = 64 * 1024;
const CLIENT_QUEUE_CAPACITY: usize = 1_024;

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

struct ServerState {
    sessions: BTreeMap<SessionId, SharedPane>,
    reserved_sessions: BTreeSet<SessionId>,
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
    // Set while a foreground-process poll is already scheduled for this pane so
    // a burst of input coalesces into a single in-flight poller thread instead
    // of spawning one thread per keystroke.
    foreground_poll_scheduled: Arc<AtomicBool>,
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
        while self.sessions.contains_key(&SessionId(self.next_session_id))
            || self
                .reserved_sessions
                .contains(&SessionId(self.next_session_id))
        {
            self.next_session_id += 1;
        }
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;
        id
    }

    fn reserve_session_id(&mut self, requested_id: Option<SessionId>) -> io::Result<SessionId> {
        let session = requested_id.unwrap_or_else(|| self.allocate_session_id());
        if self.sessions.contains_key(&session) || !self.reserved_sessions.insert(session) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("session {} already exists or is being created", session.0),
            ));
        }
        Ok(session)
    }

    fn release_session_reservation(&mut self, session: SessionId) {
        self.reserved_sessions.remove(&session);
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
        create_private_dir_all(parent)?;
    }
    Ok(())
}

fn create_private_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)
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

fn verify_peer_owner(stream: &UnixStream, peer_label: &str) -> io::Result<()> {
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
        foreground_poll_scheduled: Arc::new(AtomicBool::new(false)),
    }));

    Ok(SpawnedPane { pane, reader })
}

fn handle_client(stream: UnixStream, server: SharedServer) -> io::Result<()> {
    verify_peer_owner(&stream, "client")?;
    let (sender, receiver) = mpsc::sync_channel(CLIENT_QUEUE_CAPACITY);
    let client_id = server.lock().map_err(lock_error)?.allocate_client_id();
    let shutdown_handle = Arc::new(stream.try_clone()?);
    let client = ClientHandle {
        id: client_id,
        sender: sender.clone(),
        stream: Arc::clone(&shutdown_handle),
    };

    let mut writer_stream = stream.try_clone()?;
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
        let _ = client.sender.try_send(ServerMessage::Error {
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
                let _ = client.sender.try_send(ServerMessage::Sessions(vec![info]));
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
                    // The session is gone (e.g., the daemon was restarted or
                    // autospawned fresh while this client was away). Report the
                    // pane as exited so a reconnecting client stops treating it as
                    // live and can recover, instead of silently freezing on a
                    // session that no longer exists.
                    let _ = client.sender.try_send(ServerMessage::PaneExited {
                        pane: PaneId(session.0),
                        exit: ExitInfo {
                            code: 1,
                            signal: Some("server session unavailable".to_string()),
                        },
                    });
                    continue;
                };

                let (pane_info, pane_id, history, foreground_process) = {
                    let mut pane = pane.lock().map_err(lock_error)?;
                    pane.resize(rows, cols)?;
                    pane.attach_client(client.clone());
                    let foreground_process = pane.refresh_foreground_process();
                    (
                        pane.pane_info(),
                        pane.pane,
                        pane.raw_history.clone(),
                        foreground_process,
                    )
                };

                let _ = client.sender.try_send(ServerMessage::Attached {
                    session,
                    panes: vec![pane_info],
                });
                let _ = client.sender.try_send(ServerMessage::ForegroundProcess {
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

                // Take the child out under a brief lock, then kill+reap with the
                // pane lock released so the blocking wait never stalls the reader
                // thread. Remove the session by identity so a recycled id created
                // in the meantime is never torn down by mistake.
                let (session, child) = {
                    let mut target = target.lock().map_err(lock_error)?;
                    (target.session, target.take_child())
                };
                match kill_and_reap(child) {
                    Ok(()) => {
                        server
                            .lock()
                            .map_err(lock_error)?
                            .remove_session_if_same(session, &target);
                    }
                    Err(error) => {
                        let _ = client.sender.try_send(ServerMessage::Error {
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
        let _ = client.sender.try_send(ServerMessage::Error {
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

fn create_session(server: &SharedServer, spec: SessionCreateSpec) -> io::Result<SharedPane> {
    create_session_with_spawner(server, spec, spawn_pane)
}

fn create_session_with_spawner(
    server: &SharedServer,
    spec: SessionCreateSpec,
    spawn: impl FnOnce(SessionId, PaneSpawnSpec) -> io::Result<SpawnedPane>,
) -> io::Result<SharedPane> {
    let session = {
        let mut server = server.lock().map_err(lock_error)?;
        if let Some(requested_id) = spec.requested_id {
            if let Some(existing) = server.sessions.get(&requested_id).cloned() {
                return Ok(existing);
            }
        }
        server.reserve_session_id(spec.requested_id)?
    };

    let spawned = match spawn(session, spec.pane) {
        Ok(spawned) => spawned,
        Err(error) => {
            if let Ok(mut server) = server.lock() {
                server.release_session_reservation(session);
            }
            return Err(error);
        }
    };

    {
        let mut server = server.lock().map_err(lock_error)?;
        server.release_session_reservation(session);
        if server.sessions.contains_key(&session) {
            let child = spawned
                .pane
                .lock()
                .ok()
                .and_then(|mut pane| pane.take_child());
            drop(server);
            let _ = kill_and_reap(child);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("session {} was created concurrently", session.0),
            ));
        }
        server.sessions.insert(session, Arc::clone(&spawned.pane));
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
                    let dropped = broadcast_pty_output(pane_id, bytes, &clients);
                    remove_pane_clients(&pane, &dropped);
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
        let _ = broadcast_exit(pane_id, exit, &clients);
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
        if !client.try_deliver(ServerMessage::PtyScrollback {
            pane,
            bytes: Vec::new(),
        }) {
            client.disconnect();
        }
        return;
    }

    for chunk in bytes.chunks(RAW_HISTORY_CHUNK_BYTES) {
        if !client.try_deliver(ServerMessage::PtyScrollback {
            pane,
            bytes: chunk.to_vec(),
        }) {
            client.disconnect();
            break;
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

fn foreground_process_update(
    pane: &SharedPane,
) -> Option<(PaneId, ForegroundProcessInfo, Vec<ClientHandle>)> {
    let mut pane = pane.lock().ok()?;
    let process = pane.refresh_foreground_process_if_changed()?;
    Some((pane.pane, process, pane.clients.clone()))
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
    clients: &[ClientHandle],
    mut deliver: impl FnMut(&ClientHandle) -> bool,
) -> Vec<ClientId> {
    let mut dropped = Vec::new();
    for client in clients {
        if !deliver(client) {
            client.disconnect();
            dropped.push(client.id);
        }
    }
    dropped
}

fn remove_pane_clients(pane: &SharedPane, dropped: &[ClientId]) {
    if dropped.is_empty() {
        return;
    }
    if let Ok(mut pane) = pane.lock() {
        pane.clients.retain(|client| !dropped.contains(&client.id));
    }
}

fn broadcast_exit(pane: PaneId, exit: ExitInfo, clients: &[ClientHandle]) -> Vec<ClientId> {
    deliver_to_clients(clients, |client| {
        client.try_deliver(ServerMessage::PaneExited {
            pane,
            exit: exit.clone(),
        })
    })
}

fn broadcast_pty_output(pane: PaneId, bytes: Vec<u8>, clients: &[ClientHandle]) -> Vec<ClientId> {
    deliver_to_clients(clients, |client| {
        client.try_deliver(ServerMessage::PtyOutput {
            pane,
            bytes: bytes.clone(),
        })
    })
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

    fn take_child(&mut self) -> Option<Box<dyn Child + Send + Sync>> {
        self.child.take()
    }

    /// Attach `client`, taking over from any other client (single-attach with
    /// takeover). This lets a reconnecting client re-attach even when its
    /// previous, now-dead connection is still listed here and has not been
    /// reaped yet, instead of being rejected as already attached.
    fn attach_client(&mut self, client: ClientHandle) {
        self.clients.retain(|existing| existing.id == client.id);
        if !self.clients.iter().any(|existing| existing.id == client.id) {
            self.clients.push(client);
        }
    }

    fn append_raw_history(&mut self, bytes: &[u8]) {
        self.raw_history.extend_from_slice(bytes);
        let overflow = self.raw_history.len().saturating_sub(RAW_HISTORY_MAX_BYTES);
        if overflow > 0 {
            self.raw_history.drain(..overflow);
        }
    }
}

/// Kill and reap a PTY child. `wait` can block, so this must run with the
/// `PaneState` mutex released — the per-pane reader thread needs that lock to
/// keep draining output and would otherwise stall behind us.
fn kill_and_reap(child: Option<Box<dyn Child + Send + Sync>>) -> io::Result<()> {
    let Some(mut child) = child else {
        return Ok(());
    };
    child.kill()?;
    let _ = child.wait();
    Ok(())
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
    fn peer_owner_check_accepts_same_user_socket_pair() {
        let (client, _server) = UnixStream::pair().expect("create socket pair");

        verify_peer_owner(&client, "test client").expect("same uid peer is accepted");
        assert!(uid_matches_peer(current_euid(), current_euid()));
        assert!(!uid_matches_peer(
            current_euid().saturating_add(1),
            current_euid()
        ));
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
    fn session_allocation_skips_reserved_ids() {
        let mut server = ServerState::default();
        server
            .reserve_session_id(Some(SessionId(1)))
            .expect("reserve first session");

        assert_eq!(server.allocate_session_id(), SessionId(2));
    }

    #[test]
    fn duplicate_requested_session_reservation_is_rejected() {
        let mut server = ServerState::default();
        server
            .reserve_session_id(Some(SessionId(7)))
            .expect("reserve requested session");

        let error = server
            .reserve_session_id(Some(SessionId(7)))
            .expect_err("duplicate reservation should fail");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn reservation_is_released_when_spawn_fails() {
        let server = Arc::new(Mutex::new(ServerState::default()));
        let error = match create_session_with_spawner(
            &server,
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
            |_session, _spec| Err(io::Error::other("injected spawn failure")),
        ) {
            Ok(_) => panic!("injected spawn failure should fail"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), io::ErrorKind::Other);
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
                pane: PaneId(1),
                bytes: Vec::new(),
            })
            .expect("prime the client queue");

        // Returns promptly (the test would hang on a blocking send) and reports
        // the slow client for eviction.
        let dropped =
            broadcast_pty_output(PaneId(1), b"more".to_vec(), std::slice::from_ref(&client));

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

        let dropped =
            broadcast_pty_output(PaneId(3), b"data".to_vec(), std::slice::from_ref(&client));

        assert!(dropped.is_empty());
        assert!(matches!(
            receiver.recv(),
            Ok(ServerMessage::PtyOutput { pane, bytes }) if pane == PaneId(3) && bytes == b"data"
        ));
    }

    #[test]
    fn attaching_a_new_client_takes_over_from_the_previous_one() {
        let mut pane = test_pane_state();

        pane.attach_client(test_client(1));
        assert_eq!(pane.clients.len(), 1);
        assert_eq!(pane.clients[0].id, 1);

        // A second client takes over: the previous one is evicted.
        pane.attach_client(test_client(2));
        assert_eq!(pane.clients.len(), 1);
        assert_eq!(pane.clients[0].id, 2);

        // Re-attaching the same id is idempotent (no duplicate entry).
        pane.attach_client(test_client(2));
        assert_eq!(pane.clients.len(), 1);
        assert_eq!(pane.clients[0].id, 2);
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
