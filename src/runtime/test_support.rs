//! Fixtures shared by the runtime submodule tests: scripted daemons, a project
//! with a running command terminal, and private temporary paths.
//!
//! These used to be duplicated per test site because `runtime` was declared by
//! `main.rs` alone and so could not be reached from the library or from
//! `tests/` (F9).

use std::{os::unix::net::UnixListener, sync::mpsc, thread};

use mult_protocol::{
    read_message, write_message, AttachError, AttachOutcome, AttachmentLease, ClientMessage,
    ClientScopeId, OutputSequence, PaneId, ServerInstanceId, ServerMessage, SessionId,
    PROTOCOL_VERSION,
};

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    app::{App, NavItem},
    config::Config,
    model::{self, TerminalLaunch},
    pty::PtyRuntime,
    storage,
};

/// `Config` carries a private `warnings` list, so its fields cannot be
/// filled with functional-update syntax from here.
pub(super) fn config_with(mutate: impl FnOnce(&mut Config)) -> Config {
    let mut config = Config::default();
    mutate(&mut config);
    config
}

#[derive(Clone, Copy)]
pub(super) enum RestorationReply {
    Attached,
    Missing,
}

/// A daemon that attaches every pane and records every client message until
/// the client closes the socket. Unlike [`connected_restoration_runtime`]
/// the server thread outlives the first request, so a test can assert on
/// what the client sent *after* startup — join it only once the runtime has
/// been dropped.
pub(super) fn recording_attached_runtime(
    terminal: model::TerminalId,
) -> (
    PtyRuntime,
    mpsc::Receiver<ClientMessage>,
    thread::JoinHandle<()>,
    PathBuf,
) {
    let socket_path = unique_status_path("recording").with_extension("sock");
    let _ = fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind recording test socket");
    let (observed_tx, observed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept recording client");
        let hello: ClientMessage = read_message(&mut stream).expect("read client hello");
        assert!(matches!(hello, ClientMessage::Hello { .. }));
        write_message(
            &mut stream,
            &ServerMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                server_instance: ServerInstanceId::from_bytes([1; 16]),
                client_scope: ClientScopeId::from_bytes([2; 16]),
                resumed: false,
            },
        )
        .expect("write server hello");

        let lease = AttachmentLease::MIN;
        let pane = PaneId(terminal.0);
        // Ends when the client drops its socket: the test is over.
        while let Ok(message) = read_message::<ClientMessage>(&mut stream) {
            if let ClientMessage::Attach { request_id, .. } = &message {
                let request_id = *request_id;
                for reply in [
                    ServerMessage::AttachResult {
                        request_id,
                        outcome: AttachOutcome::Attached {
                            session: SessionId(terminal.0),
                            pane: mult_protocol::PaneInfo {
                                id: pane,
                                title: "recorded".to_string(),
                                rows: 40,
                                cols: 86,
                            },
                            lease,
                        },
                    },
                    ServerMessage::ReplayBegin {
                        request_id,
                        pane,
                        lease,
                        first_sequence: OutputSequence::ZERO,
                        watermark: OutputSequence::ZERO,
                        omitted_prefix_bytes: 0,
                    },
                    ServerMessage::ReplayEnd {
                        request_id,
                        pane,
                        lease,
                        watermark: OutputSequence::ZERO,
                    },
                ] {
                    write_message(&mut stream, &reply).expect("write attach reply");
                }
            }
            if observed_tx.send(message).is_err() {
                break;
            }
        }
    });
    let runtime =
        PtyRuntime::connect_to_socket(socket_path.clone()).expect("connect recording runtime");
    (runtime, observed_rx, server, socket_path)
}

pub(super) fn connected_restoration_runtime(
    terminal: model::TerminalId,
    reply: RestorationReply,
) -> (
    PtyRuntime,
    mpsc::Receiver<ClientMessage>,
    thread::JoinHandle<()>,
    PathBuf,
) {
    let socket_path = unique_status_path("restore").with_extension("sock");
    let _ = fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind restoration test socket");
    let (observed_tx, observed_rx) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept restoration client");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bound restoration server reads");
        let hello: ClientMessage = read_message(&mut stream).expect("read client hello");
        assert!(matches!(hello, ClientMessage::Hello { .. }));
        write_message(
            &mut stream,
            &ServerMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                server_instance: ServerInstanceId::from_bytes([1; 16]),
                client_scope: ClientScopeId::from_bytes([2; 16]),
                resumed: false,
            },
        )
        .expect("write server hello");
        let message: ClientMessage = read_message(&mut stream).expect("read restoration request");
        let ClientMessage::Attach { request_id, .. } = message.clone() else {
            panic!("restoration must send Attach, got {message:?}");
        };
        observed_tx
            .send(message)
            .expect("report restoration request");
        match reply {
            RestorationReply::Attached => {
                let lease = AttachmentLease::MIN;
                write_message(
                    &mut stream,
                    &ServerMessage::AttachResult {
                        request_id,
                        outcome: AttachOutcome::Attached {
                            session: SessionId(terminal.0),
                            pane: mult_protocol::PaneInfo {
                                id: PaneId(terminal.0),
                                title: "restored".to_string(),
                                rows: 40,
                                cols: 86,
                            },
                            lease,
                        },
                    },
                )
                .expect("write attach result");
                write_message(
                    &mut stream,
                    &ServerMessage::ReplayBegin {
                        request_id,
                        pane: PaneId(terminal.0),
                        lease,
                        first_sequence: OutputSequence::ZERO,
                        watermark: OutputSequence::ZERO,
                        omitted_prefix_bytes: 0,
                    },
                )
                .expect("write replay begin");
                write_message(
                    &mut stream,
                    &ServerMessage::ReplayEnd {
                        request_id,
                        pane: PaneId(terminal.0),
                        lease,
                        watermark: OutputSequence::ZERO,
                    },
                )
                .expect("write replay end");
            }
            RestorationReply::Missing => {
                write_message(
                    &mut stream,
                    &ServerMessage::AttachResult {
                        request_id,
                        outcome: AttachOutcome::Error(AttachError::SessionNotFound {
                            session: SessionId(terminal.0),
                        }),
                    },
                )
                .expect("write missing attach result");
            }
        }
    });
    let runtime =
        PtyRuntime::connect_to_socket(socket_path.clone()).expect("connect restoration runtime");
    (runtime, observed_rx, server, socket_path)
}

pub(super) fn running_command_app(command: String) -> (App, model::WorkspaceId, model::TerminalId) {
    let mut state = model::ProjectState::try_first_run().expect("first-run project");
    let workspace = state.workspaces[0].id;
    let terminal = state.workspaces[0].terminals[0].id;
    let session = state
        .terminal_mut_by_id(terminal)
        .expect("default terminal exists");
    session.restore_on_launch = true;
    session.launch = TerminalLaunch::Command(command);
    let mut app = App::new(state);
    app.select_item(NavItem::Terminal {
        workspace,
        terminal,
    });
    (app, workspace, terminal)
}

pub(super) fn unique_status_path(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "mult-status-test-{label}-{}-{nanos}.json",
        std::process::id()
    ))
}

/// A state store over a fresh private directory.
///
/// Input handling reaches the agent-launch path, which persists through the
/// locked store (B16), so tests that drive keys need one even when they
/// never save.
pub(super) fn test_state_store(label: &str) -> storage::StateStore {
    let path = unique_status_path(label)
        .with_extension("store")
        .join("state.json");
    storage::StateStore::acquire(
        storage::StatePaths::from_explicit_path(path).expect("test state path"),
    )
    .expect("acquire test state store")
}

pub(super) fn write_private_status(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)
}
