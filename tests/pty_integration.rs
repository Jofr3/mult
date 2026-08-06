#![cfg(unix)]

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use mult::{
    model::{
        ChatId, PtyKey, SessionIdentity as ModelSessionIdentity, SessionToken as ModelSessionToken,
        StateNamespace as ModelStateNamespace, TerminalId,
    },
    pty::{PtyDimensions, PtyEvent, PtyExit, PtyRuntime, PtySpawn},
};
use mult_protocol::{
    read_message, write_message, AgentGeneration, AgentKind, AgentSessionMetadata, AgentStatus,
    AgentStatusError, AgentStatusOutcome, AgentStatusQuery, AgentStatusRecord, AttachError,
    AttachOutcome, AttachmentLease, ClientMessage, CreateError, CreateOutcome, IdentityMismatch,
    LaunchSpec, LeaseOperation, LeaseRejectionReason, OutputSequence, PaneId, RequestId,
    ServerMessage, SessionId, SessionIdentity, SessionToken, StateNamespace, StopError,
    StopOutcome, AGENT_STATUS_SCHEMA_VERSION, PROTOCOL_VERSION, SOCKET_PATH_ENV,
};

/// G11: a deadline is a failure detector, not a schedule. Every wait here polls
/// for an *observable* condition and returns the moment it holds, so a generous
/// cap costs the happy path nothing and buys the suite immunity to a loaded CI
/// runner. The old 5 s cap, combined with `sleep 1`/`2`/`3` embedded in the
/// shell commands, made several tests race their own fixtures.
const INTEGRATION_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(15);
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
/// Printed by [`pty_integration_harness_runs_a_real_pty`] when the suite is not
/// skipped. CI greps for it, so a job that silently skipped every test cannot
/// report success (S9). Keep in step with `.github/workflows/ci.yml`.
const EXECUTION_SENTINEL: &str = "MULT_PTY_INTEGRATION_RAN";
/// Set by the CI job that is supposed to exercise real PTYs. With it set,
/// asking to skip is a hard failure rather than a silent no-op.
const REQUIRE_INTEGRATION_ENV: &str = "MULT_REQUIRE_PTY_INTEGRATION";

struct CapturedStderr {
    bytes: Arc<Mutex<Vec<u8>>>,
    complete: Arc<AtomicBool>,
}

impl CapturedStderr {
    fn start(mut stderr: impl Read + Send + 'static) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let complete = Arc::new(AtomicBool::new(false));
        let captured_bytes = Arc::clone(&bytes);
        let capture_complete = Arc::clone(&complete);
        let _stderr_reader = thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => captured_bytes
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .extend_from_slice(&buffer[..read]),
                    Err(error) => {
                        captured_bytes
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .extend_from_slice(
                                format!("\n[failed to capture mult-server stderr: {error}]")
                                    .as_bytes(),
                            );
                        break;
                    }
                }
            }
            capture_complete.store(true, Ordering::Release);
        });
        Self { bytes, complete }
    }

    fn snapshot(&self) -> String {
        let deadline = Instant::now() + STDERR_DRAIN_TIMEOUT;
        while !self.complete.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let bytes = self
            .bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if bytes.is_empty() {
            "<empty>".to_string()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }
}

struct ServerGuard {
    child: Child,
    socket_path: PathBuf,
    server_bin: PathBuf,
    shell: PathBuf,
    stderr: CapturedStderr,
}

impl ServerGuard {
    fn terminate(&mut self) -> Result<ExitStatus, String> {
        terminate_child(&mut self.child).map_err(|error| {
            format!(
                "failed to terminate mult-server (binary={}, socket={}, shell={}): {error}; stderr:\n{}",
                self.server_bin.display(),
                self.socket_path.display(),
                self.shell.display(),
                self.stderr.snapshot()
            )
        })
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            eprintln!("{error}");
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// S9: proof that this file actually ran.
///
/// `MULT_SKIP_PTY_INTEGRATION` turns every test here into a no-op that still
/// reports `ok`, so "24 passed" is not evidence that a single PTY was opened.
/// This test spawns a real PTY through a real daemon and prints
/// [`EXECUTION_SENTINEL`] only after that worked; CI runs the suite with
/// `--nocapture` and greps for the sentinel, so the line cannot appear unless
/// the harness genuinely exercised a PTY.
#[test]
fn pty_integration_harness_runs_a_real_pty() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start isolated mult-server fixture");
    let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("connect to isolated mult-server");
    let terminal = PtyKey::Terminal(TerminalId(7000));

    start_short_lived_command(&mut runtime, terminal, "printf 'pty is alive\\n'; exit 0");
    let observed = wait_for_terminal_exit(&mut runtime, terminal)
        .expect("the harness must be able to run a command on a real PTY");

    assert_eq!(observed.exit.code, 0);
    assert!(
        observed.output.contains("pty is alive"),
        "real PTY output: {:?}",
        observed.output
    );
    println!("{EXECUTION_SENTINEL}");
}

/// G11: this test used to race its own fixture. It needs a *live* `PtyOutput`
/// after the replay, but `printf …; exit 7` can finish before the client's
/// `Attach` lands, in which case the daemon correctly delivers every byte as
/// replay and no live output is ever produced. Widening the timeout could not
/// fix that: the awaited event does not exist in the losing interleaving.
///
/// The command now blocks on a FIFO after its first write, and the test only
/// releases it once `start` has returned — that is, once both `CreateSession`
/// and `Attach` have completed. Everything printed after the release is live
/// output by construction, and the attach's replay transaction (which always
/// ends in a `Scrollback` event, whether or not it carried bytes) has already
/// been delivered.
#[test]
fn client_receives_scrollback_output_and_real_exit_from_server_pty() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start isolated mult-server fixture");
    let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("connect to isolated mult-server");
    let terminal = PtyKey::Terminal(TerminalId(7001));
    let dir = unique_private_dir("live-output");
    let release = make_fifo(&dir.join("release.fifo"));

    start_short_lived_command(
        &mut runtime,
        terminal,
        &format!(
            "printf 'hello from pty\\n'; read -r line < {}; printf 'live after attach\\n'; exit 7",
            shell_quote_test(&release.to_string_lossy())
        ),
    );

    // Opening the write end blocks until the child opens the read end, which it
    // reaches only after its first `printf`.
    release_fifo_waiter(&release).expect("child should reach its FIFO read");

    let observed = wait_for_terminal_exit(&mut runtime, terminal)
        .expect("released command should produce live output and exit");

    assert!(
        observed.saw_scrollback,
        "attach must complete its raw scrollback replay transaction"
    );
    assert!(
        observed.saw_output,
        "output written after the attach must arrive as live PTY output"
    );
    assert_eq!(observed.exit.code, 7);
    assert_eq!(observed.exit.signal, None);
    assert!(
        observed.output.contains("hello from pty"),
        "terminal output should include command stdout: {:?}",
        observed.output
    );
    assert!(
        observed.output.contains("live after attach"),
        "terminal output should include what was written after the attach: {:?}",
        observed.output
    );
    assert!(!runtime.is_running(terminal));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn reconnect_replays_raw_scrollback_into_fresh_parser() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start isolated mult-server fixture");
    let terminal = PtyKey::Terminal(TerminalId(7003));

    {
        let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
            .expect("connect to isolated mult-server");
        // A keep-alive loop, not `sleep 2`: the session must outlive the
        // detach/reconnect below however long that takes, and the test stops it
        // explicitly at the end.
        start_short_lived_command(
            &mut runtime,
            terminal,
            "printf replayed; while :; do sleep 1; done",
        );
        wait_for_output(&mut runtime, terminal, "replayed")
            .expect("first client should receive output before detach");
    }

    let mut reconnected = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("reconnect to isolated mult-server");
    register_test_identity(&mut reconnected, terminal);
    assert_eq!(
        reconnected
            .attach_existing(terminal, PtyDimensions { rows: 6, cols: 40 })
            .expect("attach existing replay session"),
        mult::pty::AttachExistingResult::Attached
    );
    wait_for_output(&mut reconnected, terminal, "replayed")
        .expect("reattach should replay buffered raw PTY output");
    assert!(reconnected.stop(terminal).expect("stop replayed terminal"));
}

#[test]
fn second_client_takes_over_session_from_a_still_attached_client() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start isolated mult-server fixture");
    let terminal = PtyKey::Terminal(TerminalId(7005));

    // Client A attaches to a long-running session and observes its output.
    let mut client_a =
        PtyRuntime::connect_to_socket(server.socket_path.clone()).expect("connect client A");
    start_short_lived_command(
        &mut client_a,
        terminal,
        "printf takeover; while true; do sleep 1; done",
    );
    wait_for_output(&mut client_a, terminal, "takeover").expect("client A should see output");

    // Client B uses attach-only takeover. A second CreateSession with a
    // different command is correctly a correlated SessionAlreadyExists error.
    let mut client_b =
        PtyRuntime::connect_to_socket(server.socket_path.clone()).expect("connect client B");
    register_test_identity(&mut client_b, terminal);
    assert_eq!(
        client_b
            .attach_existing(terminal, PtyDimensions { rows: 6, cols: 40 })
            .expect("client B should take over the existing session"),
        mult::pty::AttachExistingResult::Attached
    );
    wait_for_output(&mut client_b, terminal, "takeover")
        .expect("takeover should replay buffered output to client B");

    assert!(client_b.stop(terminal).expect("stop after takeover"));
}

#[test]
fn reattach_after_server_restart_reports_vanished_session_as_exited() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start isolated mult-server fixture");
    let socket_path = server.socket_path.clone();
    let terminal = PtyKey::Terminal(TerminalId(7006));

    let mut runtime =
        PtyRuntime::connect_to_socket(socket_path.clone()).expect("connect to first server");
    start_short_lived_command(
        &mut runtime,
        terminal,
        "printf alive; while true; do sleep 1; done",
    );
    wait_for_output(&mut runtime, terminal, "alive").expect("see output from first server");
    assert!(runtime.is_running(terminal));

    // Restart the daemon on the same socket path: the previous session is gone.
    drop(server);
    let _server2 =
        start_isolated_server_at(socket_path).expect("restart isolated mult-server fixture");

    // Nudging the runtime makes it reconnect to the fresh server and re-attach;
    // because the session no longer exists, the server answers with PaneExited
    // and the client cleanly retires the terminal instead of freezing on it.
    let status = wait_for_terminal_exit_after_reconnect(&mut runtime, terminal)
        .expect("client should learn the session vanished after a server restart");
    assert_ne!(status.code, 0, "a vanished session is not a clean exit");
    assert!(!runtime.is_running(terminal));
}

#[test]
fn terminal_is_retained_when_daemon_delivery_is_uncertain() {
    if integration_tests_are_skipped() {
        return;
    }
    let mut server = start_isolated_server().expect("start isolated mult-server fixture");
    let terminal = PtyKey::Terminal(TerminalId(7007));

    let mut runtime =
        PtyRuntime::connect_to_socket(server.socket_path.clone()).expect("connect to server");
    start_short_lived_command(
        &mut runtime,
        terminal,
        "printf live; while true; do sleep 1; done",
    );
    wait_for_output(&mut runtime, terminal, "live").expect("see output before the daemon dies");
    assert!(runtime.is_running(terminal));

    // Kill the daemon and do NOT restart it. Integration tests never autospawn
    // (the test binary is not named `mult`, so server_executable() is None), so
    // this reproduces the "daemon gone, autospawn unavailable" case.
    server
        .terminate()
        .expect("terminate isolated mult-server within deadline");

    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let mut observed_uncertain = false;
    while Instant::now() < deadline && !observed_uncertain {
        let _ = runtime.resize(terminal, PtyDimensions { rows: 6, cols: 40 });
        observed_uncertain = runtime.drain_events().into_iter().any(|event| {
            matches!(event, PtyEvent::Error { message, .. } if message.contains("retained pending reconciliation") || message.contains("uncertain"))
        });
        thread::yield_now();
    }
    assert!(
        observed_uncertain,
        "daemon loss must be reported as uncertain"
    );
    assert!(
        runtime.is_running(terminal),
        "transport loss alone is not proof that the pane disappeared"
    );
}

#[test]
fn server_ignores_sighup_and_keeps_sessions_running() {
    if integration_tests_are_skipped() {
        return;
    }
    let mut server = start_isolated_server().expect("start isolated mult-server fixture");
    let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("connect to isolated mult-server");
    let terminal = PtyKey::Terminal(TerminalId(7004));
    let dir = unique_private_dir("sighup");
    let release = make_fifo(&dir.join("release.fifo"));

    // G11: the old fixture was `printf before; sleep 1; printf after; sleep 3`,
    // which quietly assumed the SIGHUP would be delivered within one second and
    // the whole test would finish within four. The FIFO makes "after" happen
    // strictly *because* the test released it, and strictly after the hangup.
    start_short_lived_command(
        &mut runtime,
        terminal,
        &format!(
            "printf before; read -r line < {}; printf after; while :; do sleep 1; done",
            shell_quote_test(&release.to_string_lossy())
        ),
    );
    wait_for_output(&mut runtime, terminal, "before")
        .expect("command should produce output before hangup");

    let rc = unsafe { libc::kill(server.child.id() as i32, libc::SIGHUP) };
    assert_eq!(rc, 0, "send SIGHUP to mult-server");
    assert_server_still_running(&mut server);

    release_fifo_waiter(&release).expect("child should still be waiting after the hangup");
    wait_for_output(&mut runtime, terminal, "after")
        .expect("server should keep PTY command running after SIGHUP");

    assert!(runtime.stop(terminal).expect("stop terminal after SIGHUP"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rapid_stop_restart_and_chat_runtime_ids_keep_client_registry_consistent() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start isolated mult-server fixture");
    let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("connect to isolated mult-server");
    let terminal = PtyKey::Terminal(TerminalId(7002));

    start_short_lived_command(&mut runtime, terminal, "while true; do sleep 1; done");
    assert!(runtime.is_running(terminal));
    assert!(runtime.stop(terminal).expect("stop running terminal"));
    assert!(!runtime.is_running(terminal));

    start_short_lived_command(&mut runtime, terminal, "printf restarted; exit 0");
    let restarted = wait_for_terminal_exit(&mut runtime, terminal)
        .expect("restarted command should exit naturally");
    assert_eq!(restarted.exit.code, 0);
    assert!(restarted.output.contains("restarted"));
    assert!(!runtime.is_running(terminal));

    let chat_terminal = PtyKey::ChatAgent(ChatId(77));
    start_short_lived_command(&mut runtime, chat_terminal, "printf chat-agent; exit 0");
    let chat = wait_for_terminal_exit(&mut runtime, chat_terminal)
        .expect("chat-agent runtime terminal should exit naturally");
    assert_eq!(chat.exit.code, 0);
    assert!(chat.output.contains("chat-agent"));
    assert!(!runtime.is_running(chat_terminal));
}

#[test]
fn state_namespace_collision_is_rejected_without_relaunching() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7091);
    let identity = identity_for_session(session);
    let wrong_namespace = SessionIdentity {
        namespace: StateNamespace::from_bytes([0x52; 16]).unwrap(),
        token: identity.token,
    };
    let mut client = RawClient::connect(&server.socket_path).unwrap();

    client.send(create_message(1, session, "cat")).unwrap();
    assert!(matches!(
        client
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::CreateResult { request_id: received, .. }
                    if *received == request_id(1)
            ))
            .unwrap(),
        ServerMessage::CreateResult {
            outcome: CreateOutcome::Created { .. },
            ..
        }
    ));

    client
        .send(create_message_with_identity(
            2,
            session,
            wrong_namespace,
            None,
            "printf must-not-launch",
        ))
        .unwrap();
    assert!(matches!(
        client
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::CreateResult { request_id: received, .. }
                    if *received == request_id(2)
            ))
            .unwrap(),
        ServerMessage::CreateResult {
            outcome: CreateOutcome::Error(CreateError::IdentityMismatch {
                mismatch: IdentityMismatch::Namespace,
                ..
            }),
            ..
        }
    ));

    client
        .send(ClientMessage::ListSessions {
            namespace: wrong_namespace.namespace,
        })
        .unwrap();
    assert!(matches!(
        client.recv_next(deadline).unwrap(),
        ServerMessage::Sessions { namespace, sessions }
            if namespace == wrong_namespace.namespace && sessions.is_empty()
    ));
    client
        .send(ClientMessage::ListSessions {
            namespace: identity.namespace,
        })
        .unwrap();
    assert!(matches!(
        client.recv_next(deadline).unwrap(),
        ServerMessage::Sessions { namespace, sessions }
            if namespace == identity.namespace
                && matches!(sessions.as_slice(), [info] if info.identity == identity)
    ));

    client.send(attach_message(3, session)).unwrap();
    let attached = client.receive_attach(deadline, request_id(3)).unwrap();
    client.send(stop_message(4, &attached)).unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(
                message,
                ServerMessage::StopResult { request_id: received, .. }
                    if *received == request_id(4)
            )
        })
        .unwrap();
}

#[test]
fn wrong_session_token_cannot_stop_the_numeric_pane() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7092);
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    client.send(create_message(1, session, "cat")).unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(
                message,
                ServerMessage::CreateResult { request_id: received, .. }
                    if *received == request_id(1)
            )
        })
        .unwrap();
    client.send(attach_message(2, session)).unwrap();
    let attached = client.receive_attach(deadline, request_id(2)).unwrap();
    let wrong_token = SessionIdentity {
        namespace: test_namespace(),
        token: SessionToken::from_bytes([0xee; 16]).unwrap(),
    };

    client
        .send(stop_message_with_identity(3, &attached, wrong_token))
        .unwrap();
    assert!(matches!(
        client
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::StopResult { request_id: received, .. }
                    if *received == request_id(3)
            ))
            .unwrap(),
        ServerMessage::StopResult {
            outcome: StopOutcome::Error(StopError::IdentityMismatch {
                mismatch: IdentityMismatch::SessionToken,
                ..
            }),
            ..
        }
    ));

    client
        .send(ClientMessage::Input {
            pane: attached.pane,
            lease: attached.lease,
            bytes: b"still-running\n".to_vec(),
        })
        .unwrap();
    client
        .receive_output_until(deadline, attached.pane, attached.lease, b"still-running")
        .expect("wrong token did not mutate the pane");
    client.send(stop_message(4, &attached)).unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(
                message,
                ServerMessage::StopResult { request_id: received, .. }
                    if *received == request_id(4)
            )
        })
        .unwrap();
}

#[test]
fn wrong_session_token_attach_cannot_take_over_or_resize_the_numeric_pane() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7098);
    let identity = identity_for_session(session);
    let mut owner = RawClient::connect(&server.socket_path).unwrap();
    owner.send(create_message(1, session, "cat")).unwrap();
    owner
        .recv_matching(deadline, |message| {
            matches!(
                message,
                ServerMessage::CreateResult { request_id: received, .. }
                    if *received == request_id(1)
            )
        })
        .unwrap();
    owner.send(attach_message(2, session)).unwrap();
    let attached = owner.receive_attach(deadline, request_id(2)).unwrap();

    let mut attacker = RawClient::connect(&server.socket_path).unwrap();
    let wrong_identity = SessionIdentity {
        namespace: identity.namespace,
        token: SessionToken::from_bytes([0xed; 16]).unwrap(),
    };
    attacker
        .send(attach_message_with_identity(1, session, wrong_identity))
        .unwrap();
    assert!(matches!(
        attacker
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::AttachResult { request_id: received, .. }
                    if *received == request_id(1)
            ))
            .unwrap(),
        ServerMessage::AttachResult {
            outcome: AttachOutcome::Error(AttachError::IdentityMismatch {
                mismatch: IdentityMismatch::SessionToken,
                ..
            }),
            ..
        }
    ));

    owner
        .send(ClientMessage::Input {
            pane: attached.pane,
            lease: attached.lease,
            bytes: b"owner-retained\n".to_vec(),
        })
        .unwrap();
    owner
        .receive_output_until(deadline, attached.pane, attached.lease, b"owner-retained")
        .expect("wrong identity attach did not take over the owner");
    owner.send(stop_message(3, &attached)).unwrap();
    owner
        .recv_matching(deadline, |message| {
            matches!(
                message,
                ServerMessage::StopResult { request_id: received, .. }
                    if *received == request_id(3)
            )
        })
        .unwrap();
}

#[test]
fn wrong_agent_status_schema_is_rejected() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7093);
    let identity = identity_for_session(session);
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    create_agent_session(&mut client, deadline, session, identity);
    let mut record = test_agent_record(identity, AgentStatus::Running);
    record.schema_version += 1;
    client
        .send(ClientMessage::UpdateAgentStatus {
            request_id: request_id(2),
            record,
        })
        .unwrap();
    assert!(matches!(
        receive_status_result(&mut client, deadline, 2),
        AgentStatusOutcome::Error(AgentStatusError::WrongSchema { .. })
    ));
    stop_unattached_session(&mut client, deadline, session, 3, 4);
}

#[test]
fn wrong_chat_agent_and_token_status_updates_are_rejected() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7094);
    let identity = identity_for_session(session);
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    create_agent_session(&mut client, deadline, session, identity);

    let mut wrong_chat = test_agent_record(identity, AgentStatus::Running);
    wrong_chat.chat_id += 1;
    client
        .send(ClientMessage::UpdateAgentStatus {
            request_id: request_id(2),
            record: wrong_chat,
        })
        .unwrap();
    assert!(matches!(
        receive_status_result(&mut client, deadline, 2),
        AgentStatusOutcome::Error(AgentStatusError::WrongChat { .. })
    ));

    let mut wrong_agent = test_agent_record(identity, AgentStatus::Running);
    wrong_agent.agent = AgentKind::ClaudeCode;
    client
        .send(ClientMessage::UpdateAgentStatus {
            request_id: request_id(3),
            record: wrong_agent,
        })
        .unwrap();
    assert!(matches!(
        receive_status_result(&mut client, deadline, 3),
        AgentStatusOutcome::Error(AgentStatusError::WrongAgent { .. })
    ));

    let mut wrong_token = test_agent_record(identity, AgentStatus::Running);
    wrong_token.identity.token = SessionToken::from_bytes([0xef; 16]).unwrap();
    client
        .send(ClientMessage::UpdateAgentStatus {
            request_id: request_id(4),
            record: wrong_token,
        })
        .unwrap();
    assert!(matches!(
        receive_status_result(&mut client, deadline, 4),
        AgentStatusOutcome::Error(AgentStatusError::IdentityMismatch(
            IdentityMismatch::SessionToken
        ))
    ));
    stop_unattached_session(&mut client, deadline, session, 5, 6);
}

#[test]
fn stale_agent_generation_is_rejected() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7095);
    let identity = identity_for_session(session);
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    create_agent_session(&mut client, deadline, session, identity);
    let mut record = test_agent_record(identity, AgentStatus::Running);
    record.generation = AgentGeneration::from_bytes([0x62; 16]).unwrap();
    client
        .send(ClientMessage::UpdateAgentStatus {
            request_id: request_id(2),
            record,
        })
        .unwrap();
    assert!(matches!(
        receive_status_result(&mut client, deadline, 2),
        AgentStatusOutcome::Error(AgentStatusError::StaleGeneration { .. })
    ));

    let metadata = test_agent_metadata();
    client
        .send(ClientMessage::GetAgentStatus {
            request_id: request_id(3),
            query: AgentStatusQuery {
                schema_version: AGENT_STATUS_SCHEMA_VERSION,
                identity,
                chat_id: metadata.chat_id,
                agent: metadata.agent,
                generation: AgentGeneration::from_bytes([0x62; 16]).unwrap(),
            },
        })
        .unwrap();
    assert!(matches!(
        receive_status_result(&mut client, deadline, 3),
        AgentStatusOutcome::Error(AgentStatusError::StaleGeneration { .. })
    ));
    stop_unattached_session(&mut client, deadline, session, 4, 5);
}

#[test]
fn late_running_status_cannot_overwrite_a_final_failure() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7096);
    let identity = identity_for_session(session);
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    create_agent_session(&mut client, deadline, session, identity);
    for (request, status) in [(2, AgentStatus::Failed), (3, AgentStatus::Running)] {
        client
            .send(ClientMessage::UpdateAgentStatus {
                request_id: request_id(request),
                record: test_agent_record(identity, status),
            })
            .unwrap();
    }
    assert!(matches!(
        receive_status_result(&mut client, deadline, 2),
        AgentStatusOutcome::Updated(AgentStatusRecord {
            status: AgentStatus::Failed,
            ..
        })
    ));
    assert!(matches!(
        receive_status_result(&mut client, deadline, 3),
        AgentStatusOutcome::Error(AgentStatusError::FinalStatusConflict {
            current: AgentStatus::Failed,
            attempted: AgentStatus::Running,
        })
    ));
    stop_unattached_session(&mut client, deadline, session, 4, 5);
}

#[test]
fn final_agent_status_remains_visible_after_client_crash_and_reconnect() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7097);
    let identity = identity_for_session(session);
    let mut first = RawClient::connect(&server.socket_path).unwrap();
    create_agent_session(&mut first, deadline, session, identity);
    first
        .send(ClientMessage::UpdateAgentStatus {
            request_id: request_id(2),
            record: test_agent_record(identity, AgentStatus::Failed),
        })
        .unwrap();
    assert!(matches!(
        receive_status_result(&mut first, deadline, 2),
        AgentStatusOutcome::Updated(_)
    ));
    drop(first);

    let mut reconnected = RawClient::connect(&server.socket_path).unwrap();
    let metadata = test_agent_metadata();
    reconnected
        .send(ClientMessage::GetAgentStatus {
            request_id: request_id(1),
            query: AgentStatusQuery {
                schema_version: AGENT_STATUS_SCHEMA_VERSION,
                identity,
                chat_id: metadata.chat_id,
                agent: metadata.agent,
                generation: metadata.generation,
            },
        })
        .unwrap();
    assert!(matches!(
        receive_status_result(&mut reconnected, deadline, 1),
        AgentStatusOutcome::Current(Some(AgentStatusRecord {
            status: AgentStatus::Failed,
            ..
        }))
    ));
    stop_unattached_session(&mut reconnected, deadline, session, 2, 3);
    reconnected
        .send(ClientMessage::GetAgentStatus {
            request_id: request_id(4),
            query: AgentStatusQuery {
                schema_version: AGENT_STATUS_SCHEMA_VERSION,
                identity,
                chat_id: metadata.chat_id,
                agent: metadata.agent,
                generation: metadata.generation,
            },
        })
        .unwrap();
    assert!(matches!(
        receive_status_result(&mut reconnected, deadline, 4),
        AgentStatusOutcome::Current(Some(AgentStatusRecord {
            status: AgentStatus::Failed,
            ..
        }))
    ));
}

#[test]
fn raw_stateful_requests_remain_correlated_across_two_panes() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let mut client = RawClient::connect(&server.socket_path).expect("connect raw client");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session_a = SessionId(7101);
    let session_b = SessionId(7102);

    client
        .send(create_message(1, session_a, "cat"))
        .expect("pipeline create A");
    client
        .send(create_message(2, session_b, "cat"))
        .expect("pipeline create B");
    for (request, session) in [(1, session_a), (2, session_b)] {
        assert!(matches!(
            client
                .recv_matching(deadline, |message| {
                    matches!(message, ServerMessage::CreateResult { request_id: received, .. } if *received == request_id(request))
                })
                .expect("correlated create result"),
            ServerMessage::CreateResult {
                request_id: received,
                outcome: CreateOutcome::Created { session: ref info },
            } if received == request_id(request) && info.id == session
        ));
    }

    client
        .send(attach_message(3, session_a))
        .expect("pipeline attach A");
    client
        .send(attach_message(4, session_b))
        .expect("pipeline attach B");
    let attach_a = client
        .receive_attach(deadline, request_id(3))
        .expect("attach A");
    let attach_b = client
        .receive_attach(deadline, request_id(4))
        .expect("attach B");
    assert_eq!(attach_a.pane, PaneId(session_a.0));
    assert_eq!(attach_b.pane, PaneId(session_b.0));

    client
        .send(ClientMessage::Input {
            pane: attach_b.pane,
            lease: attach_b.lease,
            bytes: b"pane-b-marker\n".to_vec(),
        })
        .expect("input B");
    client
        .receive_output_until(deadline, attach_b.pane, attach_b.lease, b"pane-b-marker")
        .expect("unrelated pane B output");

    client
        .send(stop_message(5, &attach_a))
        .expect("pipeline stop A");
    client
        .send(stop_message(6, &attach_b))
        .expect("pipeline stop B");
    for request in [5, 6] {
        assert!(matches!(
            client
                .recv_matching(deadline, |message| matches!(
                    message,
                    ServerMessage::StopResult { request_id: received, .. }
                        if *received == request_id(request)
                ))
                .expect("correlated stop result"),
            ServerMessage::StopResult { request_id: received, outcome: StopOutcome::Stopped { .. } }
                if received == request_id(request)
        ));
    }
}

#[test]
fn pane_a_attach_error_does_not_abort_pane_b() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let mut client = RawClient::connect(&server.socket_path).expect("connect raw client");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session_b = SessionId(7112);
    client
        .send(create_message(1, session_b, "printf pane-b-ready; cat"))
        .unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::CreateResult { request_id: received, .. } if *received == request_id(1))
        })
        .expect("create B");

    client.send(attach_message(2, SessionId(7999))).unwrap();
    client.send(attach_message(3, session_b)).unwrap();
    assert!(matches!(
        client
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::AttachResult { request_id: received, .. }
                    if *received == request_id(2)
            ))
            .expect("pane A scoped failure"),
        ServerMessage::AttachResult {
            outcome: AttachOutcome::Error(AttachError::SessionNotFound { .. }),
            ..
        }
    ));
    let attached = client
        .receive_attach(deadline, request_id(3))
        .expect("pane B attach succeeds");
    assert_eq!(attached.pane, PaneId(session_b.0));
    client.send(stop_message(4, &attached)).expect("stop B");
    client
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::StopResult { request_id: received, .. } if *received == request_id(4))
        })
        .expect("stop B result");
}

#[test]
fn takeover_rejects_every_old_lease_mutation() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7121);
    let mut old = RawClient::connect(&server.socket_path).expect("connect old owner");
    old.send(create_message(1, session, "cat")).unwrap();
    old.recv_matching(deadline, |message| {
        matches!(message, ServerMessage::CreateResult { request_id: received, .. } if *received == request_id(1))
    })
    .unwrap();
    old.send(attach_message(2, session)).unwrap();
    let displaced = old.receive_attach(deadline, request_id(2)).unwrap();
    old.send(attach_message(2, session)).unwrap();
    let exact_retry = old.receive_attach(deadline, request_id(2)).unwrap();
    assert_eq!(
        (exact_retry.pane, exact_retry.lease),
        (displaced.pane, displaced.lease),
        "exact attach retry replays a complete transaction with the same lease"
    );

    let mut current = RawClient::connect(&server.socket_path).expect("connect new owner");
    current.send(attach_message(1, session)).unwrap();
    let replacement = current.receive_attach(deadline, request_id(1)).unwrap();
    assert_ne!(displaced.lease, replacement.lease);
    assert!(matches!(
        old.recv_matching(deadline, |message| matches!(
            message,
            ServerMessage::TakenOver { pane, lease }
                if *pane == displaced.pane && *lease == displaced.lease
        ))
        .expect("old owner receives TakenOver"),
        ServerMessage::TakenOver { .. }
    ));

    for message in [
        ClientMessage::Input {
            pane: displaced.pane,
            lease: displaced.lease,
            bytes: b"stale-input\n".to_vec(),
        },
        ClientMessage::Paste {
            pane: displaced.pane,
            lease: displaced.lease,
            bytes: b"stale-paste\n".to_vec(),
        },
        ClientMessage::Resize {
            pane: displaced.pane,
            lease: displaced.lease,
            rows: 77,
            cols: 99,
        },
        ClientMessage::Detach {
            pane: displaced.pane,
            lease: displaced.lease,
        },
    ] {
        old.send(message).unwrap();
    }
    old.send(stop_message(3, &displaced)).unwrap();

    let mut rejected = Vec::new();
    while rejected.len() < 4 {
        let message = old
            .recv_matching(deadline, |message| {
                matches!(
                    message,
                    ServerMessage::LeaseRejected { pane, lease, .. }
                        if *pane == displaced.pane && *lease == displaced.lease
                )
            })
            .expect("stale mutation rejection");
        if let ServerMessage::LeaseRejected { operation, .. } = message {
            rejected.push(operation);
        }
    }
    rejected.sort_by_key(|operation| *operation as u8);
    assert_eq!(
        rejected,
        [
            LeaseOperation::Input,
            LeaseOperation::Paste,
            LeaseOperation::Resize,
            LeaseOperation::Detach,
        ]
    );
    assert!(matches!(
        old.recv_matching(deadline, |message| matches!(
            message,
            ServerMessage::StopResult { request_id: received, .. }
                if *received == request_id(3)
        ))
        .expect("stale stop rejection"),
        ServerMessage::StopResult {
            outcome: StopOutcome::Error(StopError::LeaseRejected(
                LeaseRejectionReason::NotOwner | LeaseRejectionReason::StaleLease
            )),
            ..
        }
    ));
    old.send(attach_message(2, session)).unwrap();
    assert!(matches!(
        old.recv_matching(deadline, |message| matches!(
            message,
            ServerMessage::AttachResult { request_id: received, .. }
                if *received == request_id(2)
        ))
        .expect("taken-over cached attach is superseded"),
        ServerMessage::AttachResult {
            outcome: AttachOutcome::Error(AttachError::Superseded),
            ..
        }
    ));

    current
        .send(ClientMessage::Input {
            pane: replacement.pane,
            lease: replacement.lease,
            bytes: b"current-owner-marker\n".to_vec(),
        })
        .unwrap();
    current
        .receive_output_until(
            deadline,
            replacement.pane,
            replacement.lease,
            b"current-owner-marker",
        )
        .expect("current owner still controls pane");
    current.send(stop_message(2, &replacement)).unwrap();
    current
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::StopResult { request_id: received, .. } if *received == request_id(2))
        })
        .unwrap();
}

#[test]
fn explicit_detach_invalidates_cached_attach_lease() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let session = SessionId(7125);
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    client.send(create_message(1, session, "cat")).unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::CreateResult { request_id: received, .. } if *received == request_id(1))
        })
        .unwrap();
    client.send(attach_message(2, session)).unwrap();
    let detached = client.receive_attach(deadline, request_id(2)).unwrap();
    client
        .send(ClientMessage::Detach {
            pane: detached.pane,
            lease: detached.lease,
        })
        .unwrap();
    client
        .send(ClientMessage::ListSessions {
            namespace: test_namespace(),
        })
        .unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::Sessions { namespace, .. } if *namespace == test_namespace())
        })
        .expect("detach processing barrier");
    client.send(attach_message(2, session)).unwrap();
    assert!(matches!(
        client
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::AttachResult { request_id: received, .. }
                    if *received == request_id(2)
            ))
            .unwrap(),
        ServerMessage::AttachResult {
            outcome: AttachOutcome::Error(AttachError::Superseded),
            ..
        }
    ));
    client.send(attach_message(3, session)).unwrap();
    let replacement = client.receive_attach(deadline, request_id(3)).unwrap();
    assert_ne!(replacement.lease, detached.lease);
    client.send(stop_message(4, &replacement)).unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::StopResult { request_id: received, .. } if *received == request_id(4))
        })
        .unwrap();
}

#[test]
fn numbered_attach_replay_and_live_output_are_exactly_contiguous() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + Duration::from_secs(10);
    let session = SessionId(7131);
    let command = "i=0; while IFS= read -r n; do while [ \"$i\" -lt \"$n\" ]; do printf 'SEQ:%08d\\n' \"$i\"; i=$((i+1)); done; done";
    let mut first = RawClient::connect(&server.socket_path).unwrap();
    first.send(create_message(1, session, command)).unwrap();
    first
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::CreateResult { request_id: received, .. } if *received == request_id(1))
        })
        .unwrap();
    first.send(attach_message(2, session)).unwrap();
    let first_attachment = first.receive_attach(deadline, request_id(2)).unwrap();
    first
        .send(ClientMessage::Input {
            pane: first_attachment.pane,
            lease: first_attachment.lease,
            bytes: b"20\n".to_vec(),
        })
        .unwrap();
    first
        .receive_output_until(
            deadline,
            first_attachment.pane,
            first_attachment.lease,
            b"SEQ:00000019",
        )
        .expect("initial numbered history");

    let mut second = RawClient::connect(&server.socket_path).unwrap();
    second.send(attach_message(1, session)).unwrap();
    let replayed = second.receive_attach(deadline, request_id(1)).unwrap();
    assert_replay_is_contiguous(&replayed);
    second
        .send(ClientMessage::Input {
            pane: replayed.pane,
            lease: replayed.lease,
            bytes: b"40\n".to_vec(),
        })
        .unwrap();
    let mut combined = replayed.replay_bytes.clone();
    let mut expected = replayed.watermark;
    while !combined
        .windows(b"SEQ:00000039".len())
        .any(|window| window == b"SEQ:00000039")
    {
        let message = second
            .recv_matching(deadline, |message| {
                matches!(
                    message,
                    ServerMessage::PtyOutput { pane, lease, .. }
                        if *pane == replayed.pane && *lease == replayed.lease
                )
            })
            .expect("numbered live output");
        let ServerMessage::PtyOutput {
            sequence, bytes, ..
        } = message
        else {
            unreachable!()
        };
        assert_eq!(
            sequence, expected,
            "live output must begin at the watermark"
        );
        expected = sequence.checked_add_bytes(bytes.len()).unwrap();
        combined.extend_from_slice(&bytes);
    }
    assert_eq!(
        extract_numbered_records(&combined),
        (0_u32..40).collect::<Vec<_>>()
    );
    second.send(stop_message(2, &replayed)).unwrap();
    second
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::StopResult { request_id: received, .. } if *received == request_id(2))
        })
        .unwrap();
}

#[test]
fn stop_before_child_exit_finalizes_once_without_a_stale_session() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + Duration::from_secs(10);
    let session = SessionId(7140);
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    client
        .send(create_message(
            1,
            session,
            "trap 'echo STOP-FIRST; exit 23' TERM; echo READY; while :; do read line; done",
        ))
        .unwrap();
    assert!(matches!(
        client
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::CreateResult { request_id: received, .. }
                    if *received == request_id(1)
            ))
            .unwrap(),
        ServerMessage::CreateResult {
            outcome: CreateOutcome::Created { .. },
            ..
        }
    ));
    client.send(attach_message(2, session)).unwrap();
    let attached = client.receive_attach(deadline, request_id(2)).unwrap();
    let (mut output, mut expected_sequence) =
        receive_attachment_marker(&mut client, &attached, b"READY", deadline);

    client.send(stop_message(3, &attached)).unwrap();
    let mut pane_exit = None;
    let mut exit_count = 0;
    loop {
        match client.recv_next(deadline).expect("stop-first finalization") {
            ServerMessage::PtyOutput {
                pane,
                lease,
                sequence,
                bytes,
            } if pane == attached.pane && lease == attached.lease => {
                assert_eq!(sequence, expected_sequence, "live output sequence gap");
                expected_sequence = sequence.checked_add_bytes(bytes.len()).unwrap();
                output.extend_from_slice(&bytes);
            }
            ServerMessage::ForegroundProcess { pane, lease, .. }
                if pane == attached.pane && lease == attached.lease => {}
            ServerMessage::PaneExited { pane, lease, exit }
                if pane == attached.pane && lease == attached.lease =>
            {
                exit_count += 1;
                assert_eq!(exit_count, 1, "duplicate PaneExited");
                assert!(
                    output
                        .windows(b"STOP-FIRST".len())
                        .any(|window| window == b"STOP-FIRST"),
                    "the stop marker must be drained before finalization: {output:?}"
                );
                pane_exit = Some(exit);
            }
            ServerMessage::StopResult {
                request_id: received,
                outcome: StopOutcome::Stopped { exit },
            } if received == request_id(3) => {
                assert_eq!(Some(exit), pane_exit, "PaneExited must precede StopResult");
                break;
            }
            message => panic!("unexpected stop-first message: {message:?}"),
        }
    }

    assert_session_finalized_once(&mut client, deadline, session, &attached, request_id(2));
}

#[test]
fn natural_exit_before_stop_returns_already_absent_without_a_stale_session() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + Duration::from_secs(10);
    let session = SessionId(7141);
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    client
        .send(create_message(
            1,
            session,
            "echo READY; read line; echo NATURAL-FIRST; exit 0",
        ))
        .unwrap();
    assert!(matches!(
        client
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::CreateResult { request_id: received, .. }
                    if *received == request_id(1)
            ))
            .unwrap(),
        ServerMessage::CreateResult {
            outcome: CreateOutcome::Created { .. },
            ..
        }
    ));
    client.send(attach_message(2, session)).unwrap();
    let attached = client.receive_attach(deadline, request_id(2)).unwrap();
    let (mut output, mut expected_sequence) =
        receive_attachment_marker(&mut client, &attached, b"READY", deadline);
    client
        .send(ClientMessage::Input {
            pane: attached.pane,
            lease: attached.lease,
            bytes: b"release\n".to_vec(),
        })
        .unwrap();

    loop {
        match client
            .recv_next(deadline)
            .expect("natural-first finalization")
        {
            ServerMessage::PtyOutput {
                pane,
                lease,
                sequence,
                bytes,
            } if pane == attached.pane && lease == attached.lease => {
                assert_eq!(sequence, expected_sequence, "live output sequence gap");
                expected_sequence = sequence.checked_add_bytes(bytes.len()).unwrap();
                output.extend_from_slice(&bytes);
            }
            ServerMessage::ForegroundProcess { pane, lease, .. }
                if pane == attached.pane && lease == attached.lease => {}
            ServerMessage::PaneExited { pane, lease, .. }
                if pane == attached.pane && lease == attached.lease =>
            {
                assert!(
                    output
                        .windows(b"NATURAL-FIRST".len())
                        .any(|window| window == b"NATURAL-FIRST"),
                    "natural output must be drained before finalization: {output:?}"
                );
                break;
            }
            message => panic!("unexpected natural-first message: {message:?}"),
        }
    }

    client.send(stop_message(3, &attached)).unwrap();
    assert!(matches!(
        client.recv_next(deadline).expect("already-absent stop result"),
        ServerMessage::StopResult {
            request_id: received,
            outcome: StopOutcome::AlreadyAbsent,
        } if received == request_id(3)
    ));
    assert_session_finalized_once(&mut client, deadline, session, &attached, request_id(2));
}

#[test]
fn stop_kills_pipeline_and_sigterm_ignoring_descendant() {
    if integration_tests_are_skipped() {
        return;
    }
    let server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + Duration::from_secs(10);
    let dir = unique_private_dir("process-group");
    let fifo = dir.join("liveness.fifo");
    let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0, "mkfifo");
    let mut liveness = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .expect("open liveness reader");
    let quoted_fifo = shell_quote_test(&fifo.to_string_lossy());
    let command = format!(
        "exec 9>{quoted_fifo}; (trap '' HUP TERM; printf READY >&9; while :; do sleep 60; done) | cat"
    );
    let session = SessionId(7151);
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    client.send(create_message(1, session, &command)).unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::CreateResult { request_id: received, .. } if *received == request_id(1))
        })
        .unwrap();
    client.send(attach_message(2, session)).unwrap();
    let attached = client.receive_attach(deadline, request_id(2)).unwrap();
    wait_for_fifo_token(&mut liveness, b"READY", deadline).expect("descendant ready");

    client.send(stop_message(3, &attached)).unwrap();
    assert!(matches!(
        client
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::StopResult { request_id: received, .. }
                    if *received == request_id(3)
            ))
            .expect("group stop finalized"),
        ServerMessage::StopResult {
            outcome: StopOutcome::Stopped { .. },
            ..
        }
    ));
    wait_for_fifo_eof(&mut liveness, deadline)
        .expect("all pipeline and descendant liveness descriptors closed");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn daemon_shutdown_uses_group_finalization_and_reaps_sessions() {
    if integration_tests_are_skipped() {
        return;
    }
    let mut server = start_isolated_server().expect("start server");
    let deadline = Instant::now() + Duration::from_secs(10);
    let dir = unique_private_dir("daemon-shutdown");
    let fifo = dir.join("liveness.fifo");
    let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0, "mkfifo");
    let mut liveness = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    let command = format!(
        "exec 9>{}; trap '' HUP TERM; printf READY >&9; while :; do sleep 60; done",
        shell_quote_test(&fifo.to_string_lossy())
    );
    let mut client = RawClient::connect(&server.socket_path).unwrap();
    client
        .send(create_message(1, SessionId(7161), &command))
        .unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(message, ServerMessage::CreateResult { request_id: received, .. } if *received == request_id(1))
        })
        .unwrap();
    client.send(attach_message(2, SessionId(7161))).unwrap();
    client.receive_attach(deadline, request_id(2)).unwrap();
    wait_for_fifo_token(&mut liveness, b"READY", deadline).unwrap();

    assert_eq!(
        unsafe { libc::kill(server.child.id() as libc::pid_t, libc::SIGTERM) },
        0,
        "signal daemon shutdown"
    );
    let status = loop {
        match server.child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::yield_now(),
            Ok(None) => panic!("daemon did not finish graceful shutdown"),
            Err(error) => panic!("poll daemon shutdown: {error}"),
        }
    };
    assert!(status.success(), "graceful daemon status: {status}");
    wait_for_fifo_eof(&mut liveness, deadline).expect("daemon shutdown closed descendant FIFO");
    let _ = fs::remove_dir_all(dir);
}

#[derive(Debug, Clone)]
struct RawAttachment {
    pane: PaneId,
    lease: AttachmentLease,
    first_sequence: OutputSequence,
    watermark: OutputSequence,
    replay_ranges: Vec<(OutputSequence, usize)>,
    replay_bytes: Vec<u8>,
}

struct RawClient {
    stream: UnixStream,
    backlog: VecDeque<ServerMessage>,
}

impl RawClient {
    fn connect(socket_path: &PathBuf) -> Result<Self, String> {
        let mut stream = UnixStream::connect(socket_path)
            .map_err(|error| format!("connect {}: {error}", socket_path.display()))?;
        write_message(
            &mut stream,
            &ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                resume: None,
            },
        )
        .map_err(|error| format!("send hello: {error}"))?;
        stream
            .set_read_timeout(Some(INTEGRATION_TIMEOUT))
            .map_err(|error| error.to_string())?;
        let hello = read_message::<ServerMessage>(&mut stream)
            .map_err(|error| format!("read hello: {error}"))?;
        stream
            .set_read_timeout(None)
            .map_err(|error| error.to_string())?;
        if !matches!(
            hello,
            ServerMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                ..
            }
        ) {
            return Err(format!("unexpected hello: {hello:?}"));
        }
        Ok(Self {
            stream,
            backlog: VecDeque::new(),
        })
    }

    fn send(&mut self, message: ClientMessage) -> Result<(), String> {
        write_message(&mut self.stream, &message)
            .map_err(|error| format!("send {message:?}: {error}"))
    }

    fn recv_next(&mut self, deadline: Instant) -> Result<ServerMessage, String> {
        if let Some(message) = self.backlog.pop_front() {
            return Ok(message);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("deadline expired".to_string());
        }
        self.stream
            .set_read_timeout(Some(remaining))
            .map_err(|error| error.to_string())?;
        read_message::<ServerMessage>(&mut self.stream)
            .map_err(|error| format!("read next server message before {deadline:?}: {error}"))
    }

    fn recv_matching(
        &mut self,
        deadline: Instant,
        predicate: impl Fn(&ServerMessage) -> bool,
    ) -> Result<ServerMessage, String> {
        if let Some(index) = self.backlog.iter().position(&predicate) {
            return Ok(self.backlog.remove(index).expect("backlog index exists"));
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "deadline expired; unmatched messages: {:?}",
                    self.backlog
                ));
            }
            self.stream
                .set_read_timeout(Some(remaining))
                .map_err(|error| error.to_string())?;
            let message = read_message::<ServerMessage>(&mut self.stream).map_err(|error| {
                format!(
                    "read server message before {deadline:?}: {error}; backlog={:?}",
                    self.backlog
                )
            })?;
            if predicate(&message) {
                return Ok(message);
            }
            self.backlog.push_back(message);
        }
    }

    fn receive_output_until(
        &mut self,
        deadline: Instant,
        pane: PaneId,
        lease: AttachmentLease,
        needle: &[u8],
    ) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        while !bytes.windows(needle.len()).any(|window| window == needle) {
            let message = self.recv_matching(deadline, |message| {
                matches!(
                    message,
                    ServerMessage::PtyOutput {
                        pane: output_pane,
                        lease: output_lease,
                        ..
                    } if *output_pane == pane && *output_lease == lease
                )
            })?;
            let ServerMessage::PtyOutput { bytes: output, .. } = message else {
                unreachable!()
            };
            bytes.extend_from_slice(&output);
        }
        Ok(bytes)
    }

    fn receive_attach(
        &mut self,
        deadline: Instant,
        request: RequestId,
    ) -> Result<RawAttachment, String> {
        let result = self.recv_matching(deadline, |message| {
            matches!(message, ServerMessage::AttachResult { request_id, .. } if *request_id == request)
        })?;
        let (pane, lease) = match result {
            ServerMessage::AttachResult {
                outcome: AttachOutcome::Attached { pane, lease, .. },
                ..
            } => (pane.id, lease),
            other => return Err(format!("attach failed: {other:?}")),
        };
        let begin = self.recv_matching(deadline, |message| {
            matches!(message, ServerMessage::ReplayBegin { request_id, .. } if *request_id == request)
        })?;
        let ServerMessage::ReplayBegin {
            first_sequence,
            watermark,
            omitted_prefix_bytes,
            ..
        } = begin
        else {
            unreachable!()
        };
        if omitted_prefix_bytes != first_sequence.get() {
            return Err("replay truncation metadata did not match first sequence".to_string());
        }
        let mut replay_ranges = Vec::new();
        let mut replay_bytes = Vec::new();
        loop {
            let message = self.recv_matching(deadline, |message| {
                matches!(
                    message,
                    ServerMessage::ReplayChunk { request_id, .. }
                        | ServerMessage::ReplayEnd { request_id, .. }
                        if *request_id == request
                )
            })?;
            match message {
                ServerMessage::ReplayChunk {
                    pane: chunk_pane,
                    lease: chunk_lease,
                    sequence,
                    bytes,
                    ..
                } => {
                    if chunk_pane != pane || chunk_lease != lease {
                        return Err("replay chunk identified wrong attachment".to_string());
                    }
                    replay_ranges.push((sequence, bytes.len()));
                    replay_bytes.extend_from_slice(&bytes);
                }
                ServerMessage::ReplayEnd {
                    pane: end_pane,
                    lease: end_lease,
                    watermark: end_watermark,
                    ..
                } => {
                    if end_pane != pane || end_lease != lease || end_watermark != watermark {
                        return Err("replay end mismatch".to_string());
                    }
                    break;
                }
                _ => unreachable!(),
            }
        }
        Ok(RawAttachment {
            pane,
            lease,
            first_sequence,
            watermark,
            replay_ranges,
            replay_bytes,
        })
    }
}

fn request_id(value: u64) -> RequestId {
    RequestId::new(value).expect("non-zero request ID")
}

fn test_namespace() -> StateNamespace {
    StateNamespace::from_bytes([0x51; 16]).unwrap()
}

fn identity_for_session(session: SessionId) -> SessionIdentity {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&session.0.to_be_bytes());
    if bytes == [0; 16] {
        bytes[15] = 1;
    }
    SessionIdentity {
        namespace: test_namespace(),
        token: SessionToken::from_bytes(bytes).unwrap(),
    }
}

fn model_identity_for_key(key: PtyKey) -> ModelSessionIdentity {
    let wire_id = match key {
        PtyKey::Terminal(id) => id.0,
        PtyKey::ChatAgent(id) => id.0 | (1_u64 << 63),
    };
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&wire_id.to_be_bytes());
    if bytes == [0; 16] {
        bytes[15] = 1;
    }
    ModelSessionIdentity {
        namespace: ModelStateNamespace::from_bytes([0x51; 16]).unwrap(),
        token: ModelSessionToken::from_bytes(bytes).unwrap(),
    }
}

fn register_test_identity(runtime: &mut PtyRuntime, terminal: PtyKey) {
    runtime
        .register_session_identity(terminal, model_identity_for_key(terminal))
        .expect("register deterministic durable test identity");
}

fn create_message(request: u64, session: SessionId, command: &str) -> ClientMessage {
    create_message_with_identity(
        request,
        session,
        identity_for_session(session),
        None,
        command,
    )
}

fn create_message_with_identity(
    request: u64,
    session: SessionId,
    identity: SessionIdentity,
    agent: Option<AgentSessionMetadata>,
    command: &str,
) -> ClientMessage {
    ClientMessage::CreateSession {
        request_id: request_id(request),
        identity,
        requested_id: Some(session),
        agent,
        name: format!("integration-{}", session.0),
        cwd: None,
        env: BTreeMap::new(),
        launch: LaunchSpec::Command(command.to_string()),
        rows: 8,
        cols: 80,
    }
}

fn attach_message(request: u64, session: SessionId) -> ClientMessage {
    attach_message_with_identity(request, session, identity_for_session(session))
}

fn attach_message_with_identity(
    request: u64,
    session: SessionId,
    identity: SessionIdentity,
) -> ClientMessage {
    ClientMessage::Attach {
        request_id: request_id(request),
        identity,
        session,
        rows: 8,
        cols: 80,
    }
}

fn stop_message(request: u64, attachment: &RawAttachment) -> ClientMessage {
    stop_message_with_identity(
        request,
        attachment,
        identity_for_session(SessionId(attachment.pane.0)),
    )
}

fn stop_message_with_identity(
    request: u64,
    attachment: &RawAttachment,
    identity: SessionIdentity,
) -> ClientMessage {
    ClientMessage::Stop {
        request_id: request_id(request),
        identity,
        pane: attachment.pane,
        lease: attachment.lease,
    }
}

fn test_agent_metadata() -> AgentSessionMetadata {
    AgentSessionMetadata {
        schema_version: AGENT_STATUS_SCHEMA_VERSION,
        chat_id: 77,
        agent: AgentKind::Pi,
        generation: AgentGeneration::from_bytes([0x61; 16]).unwrap(),
    }
}

fn test_agent_record(identity: SessionIdentity, status: AgentStatus) -> AgentStatusRecord {
    let metadata = test_agent_metadata();
    AgentStatusRecord {
        schema_version: metadata.schema_version,
        identity,
        chat_id: metadata.chat_id,
        agent: metadata.agent,
        generation: metadata.generation,
        status,
    }
}

fn create_agent_session(
    client: &mut RawClient,
    deadline: Instant,
    session: SessionId,
    identity: SessionIdentity,
) {
    client
        .send(create_message_with_identity(
            1,
            session,
            identity,
            Some(test_agent_metadata()),
            "cat",
        ))
        .unwrap();
    assert!(matches!(
        client
            .recv_matching(deadline, |message| matches!(
                message,
                ServerMessage::CreateResult { request_id: received, .. }
                    if *received == request_id(1)
            ))
            .unwrap(),
        ServerMessage::CreateResult {
            outcome: CreateOutcome::Created { .. },
            ..
        }
    ));
}

fn receive_status_result(
    client: &mut RawClient,
    deadline: Instant,
    request: u64,
) -> AgentStatusOutcome {
    match client
        .recv_matching(deadline, |message| {
            matches!(
                message,
                ServerMessage::AgentStatusResult { request_id: received, .. }
                    if *received == request_id(request)
            )
        })
        .unwrap()
    {
        ServerMessage::AgentStatusResult { outcome, .. } => outcome,
        _ => unreachable!(),
    }
}

fn stop_unattached_session(
    client: &mut RawClient,
    deadline: Instant,
    session: SessionId,
    attach_request: u64,
    stop_request: u64,
) {
    client
        .send(attach_message(attach_request, session))
        .unwrap();
    let attached = client
        .receive_attach(deadline, request_id(attach_request))
        .unwrap();
    client.send(stop_message(stop_request, &attached)).unwrap();
    client
        .recv_matching(deadline, |message| {
            matches!(
                message,
                ServerMessage::StopResult { request_id: received, .. }
                    if *received == request_id(stop_request)
            )
        })
        .unwrap();
}

fn assert_replay_is_contiguous(attachment: &RawAttachment) {
    let mut expected = attachment.first_sequence;
    for (sequence, len) in &attachment.replay_ranges {
        assert_eq!(*sequence, expected, "replay gap or duplicate");
        expected = sequence.checked_add_bytes(*len).unwrap();
    }
    assert_eq!(expected, attachment.watermark);
    assert_eq!(
        attachment.replay_bytes.len() as u64,
        attachment.watermark.get() - attachment.first_sequence.get()
    );
}

fn receive_attachment_marker(
    client: &mut RawClient,
    attachment: &RawAttachment,
    marker: &[u8],
    deadline: Instant,
) -> (Vec<u8>, OutputSequence) {
    let mut output = attachment.replay_bytes.clone();
    let mut expected_sequence = attachment.watermark;
    while !output.windows(marker.len()).any(|window| window == marker) {
        match client.recv_next(deadline).expect("attachment marker") {
            ServerMessage::PtyOutput {
                pane,
                lease,
                sequence,
                bytes,
            } if pane == attachment.pane && lease == attachment.lease => {
                assert_eq!(sequence, expected_sequence, "live output sequence gap");
                expected_sequence = sequence.checked_add_bytes(bytes.len()).unwrap();
                output.extend_from_slice(&bytes);
            }
            ServerMessage::ForegroundProcess { pane, lease, .. }
                if pane == attachment.pane && lease == attachment.lease => {}
            message => panic!("unexpected message while waiting for marker: {message:?}"),
        }
    }
    (output, expected_sequence)
}

fn assert_session_finalized_once(
    client: &mut RawClient,
    deadline: Instant,
    session: SessionId,
    attachment: &RawAttachment,
    cached_attach_request: RequestId,
) {
    client
        .send(ClientMessage::ListSessions {
            namespace: test_namespace(),
        })
        .unwrap();
    loop {
        match client.recv_next(deadline).expect("session-list barrier") {
            ServerMessage::Sessions {
                namespace,
                sessions,
            } if namespace == test_namespace() => {
                assert!(
                    sessions.iter().all(|info| info.id != session),
                    "finalized session remained listed"
                );
                break;
            }
            ServerMessage::PaneExited { pane, lease, .. }
                if pane == attachment.pane && lease == attachment.lease =>
            {
                panic!("duplicate PaneExited before the session-list barrier")
            }
            ServerMessage::ForegroundProcess { pane, lease, .. }
                if pane == attachment.pane && lease == attachment.lease => {}
            message => panic!("unexpected post-finalization message: {message:?}"),
        }
    }

    client
        .send(attach_message(cached_attach_request.get(), session))
        .unwrap();
    assert!(matches!(
        client.recv_next(deadline).expect("superseded cached attach"),
        ServerMessage::AttachResult {
            request_id,
            outcome: AttachOutcome::Error(AttachError::Superseded),
        } if request_id == cached_attach_request
    ));
}

fn extract_numbered_records(bytes: &[u8]) -> Vec<u32> {
    const PREFIX: &[u8] = b"SEQ:";
    let mut records = Vec::new();
    let mut index = 0;
    while index + PREFIX.len() + 8 <= bytes.len() {
        if &bytes[index..index + PREFIX.len()] == PREFIX {
            let digits = &bytes[index + PREFIX.len()..index + PREFIX.len() + 8];
            if digits.iter().all(u8::is_ascii_digit) {
                records.push(std::str::from_utf8(digits).unwrap().parse().unwrap());
                index += PREFIX.len() + 8;
                continue;
            }
        }
        index += 1;
    }
    records
}

fn unique_private_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("mult-pty-{label}-{}-{unique}", std::process::id()));
    fs::create_dir(&path).expect("create private test directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}

fn shell_quote_test(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn make_fifo(path: &Path) -> PathBuf {
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("FIFO path");
    assert_eq!(
        unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) },
        0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    path.to_path_buf()
}

/// Unblocks a child parked in `read -r line < fifo`.
///
/// The open is the handshake: a FIFO write-open only succeeds once a reader has
/// opened the other end, so returning proves the child reached its `read` — and
/// therefore ran everything before it. `O_NONBLOCK` keeps a child that never
/// got that far from hanging the suite; a blocking open would wait forever.
fn release_fifo_waiter(path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(mut writer) => {
                return writer
                    .write_all(b"go\n")
                    .map_err(|error| format!("write release FIFO {}: {error}", path.display()));
            }
            Err(error) if error.raw_os_error() != Some(libc::ENXIO) => {
                return Err(format!("open release FIFO {}: {error}", path.display()));
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out after {INTEGRATION_TIMEOUT:?} waiting for a reader on {}: {error}",
                        path.display()
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn wait_for_fifo_token(file: &mut fs::File, token: &[u8], deadline: Instant) -> Result<(), String> {
    let mut observed = Vec::new();
    let mut buffer = [0_u8; 64];
    while Instant::now() < deadline {
        match file.read(&mut buffer) {
            Ok(0) => thread::yield_now(),
            Ok(read) => {
                observed.extend_from_slice(&buffer[..read]);
                if observed.windows(token.len()).any(|window| window == token) {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::yield_now(),
            Err(error) => return Err(format!("read liveness FIFO: {error}")),
        }
    }
    Err(format!(
        "timed out waiting for FIFO token; observed={observed:?}"
    ))
}

fn wait_for_fifo_eof(file: &mut fs::File, deadline: Instant) -> Result<(), String> {
    let mut buffer = [0_u8; 64];
    while Instant::now() < deadline {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::yield_now(),
            Err(error) => return Err(format!("read liveness FIFO: {error}")),
        }
    }
    Err("timed out waiting for all liveness FIFO writers to close".to_string())
}

struct ObservedTerminal {
    saw_scrollback: bool,
    saw_output: bool,
    exit: PtyExit,
    output: String,
}

fn start_short_lived_command(runtime: &mut PtyRuntime, terminal: PtyKey, command: &str) {
    register_test_identity(runtime, terminal);
    let mut spawn = PtySpawn::command_line(terminal, command.to_string(), None, BTreeMap::new());
    spawn.size = PtyDimensions { rows: 6, cols: 40 };
    runtime.start(spawn).expect("start PTY command");
}

fn wait_for_output(runtime: &mut PtyRuntime, terminal: PtyKey, needle: &str) -> Result<(), String> {
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let mut recent_events = VecDeque::new();
    while Instant::now() < deadline {
        for event in runtime.drain_events() {
            remember_event(&mut recent_events, &event);
            if let PtyEvent::Error {
                terminal: event_terminal,
                message,
            } = event
            {
                return Err(format!(
                    "server reported PTY error for terminal {event_terminal:?}: {message}; {}",
                    terminal_runtime_diagnostics(runtime, terminal)
                ));
            }
        }
        if runtime
            .terminal_all_lines(terminal)
            .join("\n")
            .contains(needle)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!(
        "timed out after {INTEGRATION_TIMEOUT:?} waiting for {needle:?} from {terminal:?}; {}; recent events: {recent_events:?}",
        terminal_runtime_diagnostics(runtime, terminal)
    ))
}

fn wait_for_terminal_exit(
    runtime: &mut PtyRuntime,
    terminal: PtyKey,
) -> Result<ObservedTerminal, String> {
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let mut saw_scrollback = false;
    let mut saw_output = false;
    let mut recent_events = VecDeque::new();

    while Instant::now() < deadline {
        for event in runtime.drain_events() {
            remember_event(&mut recent_events, &event);
            match event {
                PtyEvent::Scrollback {
                    terminal: event_terminal,
                    ..
                } if event_terminal == terminal => {
                    saw_scrollback = true;
                }
                PtyEvent::Output {
                    terminal: event_terminal,
                    ..
                } if event_terminal == terminal => {
                    saw_output = true;
                }
                PtyEvent::Exited {
                    terminal: event_terminal,
                    status,
                } if event_terminal == terminal => {
                    return Ok(ObservedTerminal {
                        saw_scrollback,
                        saw_output,
                        exit: status,
                        output: runtime.terminal_all_lines(terminal).join("\n"),
                    });
                }
                PtyEvent::Error {
                    terminal: event_terminal,
                    message,
                } => {
                    return Err(format!(
                        "server reported PTY error for terminal {event_terminal:?}: {message}; {}",
                        terminal_runtime_diagnostics(runtime, terminal)
                    ));
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }

    Err(format!(
        "timed out after {INTEGRATION_TIMEOUT:?} waiting for {terminal:?} to exit; saw scrollback={saw_scrollback}, saw output={saw_output}; {}; recent events: {recent_events:?}",
        terminal_runtime_diagnostics(runtime, terminal)
    ))
}

fn wait_for_terminal_exit_after_reconnect(
    runtime: &mut PtyRuntime,
    terminal: PtyKey,
) -> Result<PtyExit, String> {
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let size = PtyDimensions { rows: 6, cols: 40 };
    let mut last_resize_error = None;
    let mut recent_events = VecDeque::new();
    while Instant::now() < deadline {
        // Any send forces a transparent reconnect to the restarted server and a
        // re-attach of the still-tracked terminal; errors here are expected
        // mid-reconnect and retained for timeout diagnostics.
        if let Err(error) = runtime.resize(terminal, size) {
            last_resize_error = Some(error.to_string());
        }
        for event in runtime.drain_events() {
            remember_event(&mut recent_events, &event);
            if let PtyEvent::Exited {
                terminal: event_terminal,
                status,
            } = event
            {
                if event_terminal == terminal {
                    return Ok(status);
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!(
        "timed out after {INTEGRATION_TIMEOUT:?} waiting for {terminal:?} to exit after reconnect; last resize error: {last_resize_error:?}; {}; recent events: {recent_events:?}",
        terminal_runtime_diagnostics(runtime, terminal)
    ))
}

fn remember_event(recent_events: &mut VecDeque<String>, event: &PtyEvent) {
    const MAX_RECENT_EVENTS: usize = 12;
    if recent_events.len() == MAX_RECENT_EVENTS {
        recent_events.pop_front();
    }
    recent_events.push_back(format!("{event:?}"));
}

fn terminal_runtime_diagnostics(runtime: &PtyRuntime, terminal: PtyKey) -> String {
    const MAX_DIAGNOSTIC_LINES: usize = 12;
    let lines = runtime.terminal_all_lines(terminal);
    let recent_lines = &lines[lines.len().saturating_sub(MAX_DIAGNOSTIC_LINES)..];
    format!(
        "runtime running={}, last terminal lines={recent_lines:?}",
        runtime.is_running(terminal)
    )
}

fn assert_server_still_running(server: &mut ServerGuard) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match server.child.try_wait() {
            Ok(Some(status)) => panic!("mult-server exited after SIGHUP: {status}"),
            Ok(None) => {}
            Err(error) => panic!("failed to poll mult-server child after SIGHUP: {error}"),
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn integration_tests_are_skipped() -> bool {
    if std::env::var_os("MULT_SKIP_PTY_INTEGRATION").is_some() {
        // S9: the Nix sandbox cannot allocate PTYs, so it sets the skip and the
        // job goes green having tested nothing. That is fine as long as some
        // *other* job proves the tests ran — and the only way to prove it is to
        // make skipping fatal where it is not allowed.
        assert!(
            std::env::var_os(REQUIRE_INTEGRATION_ENV).is_none(),
            "{REQUIRE_INTEGRATION_ENV} is set, so MULT_SKIP_PTY_INTEGRATION must not be: this job \
             exists to run the PTY integration tests, not to skip them"
        );
        eprintln!("skipping PTY integration tests because MULT_SKIP_PTY_INTEGRATION is set");
        true
    } else {
        false
    }
}

fn start_isolated_server() -> Result<ServerGuard, String> {
    start_isolated_server_at(unique_socket_path())
}

fn start_isolated_server_at(socket_path: PathBuf) -> Result<ServerGuard, String> {
    let shell = integration_test_shell();
    let server_bin = option_env!("CARGO_BIN_EXE_mult-server")
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "CARGO_BIN_EXE_mult-server is unavailable; cannot start required PTY integration fixture (socket={}, shell={})",
                socket_path.display(),
                shell.display()
            )
        })?;
    let mut child = Command::new(&server_bin)
        .env(SOCKET_PATH_ENV, &socket_path)
        .env("SHELL", &shell)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!(
                "failed to spawn required mult-server fixture (binary={}, socket={}, shell={}): {error}",
                server_bin.display(),
                socket_path.display(),
                shell.display()
            )
        })?;
    let stderr_pipe = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let termination = terminate_child(&mut child);
            let _ = fs::remove_file(&socket_path);
            return Err(format!(
                "mult-server fixture stderr was not captured (binary={}, socket={}, shell={}); termination result: {termination:?}",
                server_bin.display(),
                socket_path.display(),
                shell.display()
            ));
        }
    };
    let stderr = CapturedStderr::start(stderr_pipe);

    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let mut last_connect_error = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = fs::remove_file(&socket_path);
                return Err(format!(
                    "mult-server fixture exited before accepting connections (binary={}, socket={}, shell={}, status={status}); stderr:\n{}",
                    server_bin.display(),
                    socket_path.display(),
                    shell.display(),
                    stderr.snapshot()
                ));
            }
            Ok(None) => {}
            Err(error) => {
                let termination = terminate_child(&mut child);
                let _ = fs::remove_file(&socket_path);
                return Err(format!(
                    "failed to poll mult-server fixture (binary={}, socket={}, shell={}): {error}; termination result: {termination:?}; stderr:\n{}",
                    server_bin.display(),
                    socket_path.display(),
                    shell.display(),
                    stderr.snapshot()
                ));
            }
        }

        match UnixStream::connect(&socket_path) {
            Ok(_) => {
                return Ok(ServerGuard {
                    child,
                    socket_path,
                    server_bin,
                    shell,
                    stderr,
                });
            }
            Err(error) => last_connect_error = Some(error.to_string()),
        }
        thread::sleep(Duration::from_millis(20));
    }

    let termination = terminate_child(&mut child);
    let _ = fs::remove_file(&socket_path);
    Err(format!(
        "mult-server fixture did not create a usable socket within {INTEGRATION_TIMEOUT:?} (binary={}, socket={}, shell={}, last connect error={last_connect_error:?}); termination result: {termination:?}; stderr:\n{}",
        server_bin.display(),
        socket_path.display(),
        shell.display(),
        stderr.snapshot()
    ))
}

fn terminate_child(child: &mut Child) -> Result<ExitStatus, String> {
    let initial_poll_error = match child.try_wait() {
        Ok(Some(status)) => return Ok(status),
        Ok(None) => None,
        Err(error) => Some(error),
    };

    if let Err(kill_error) = child.kill() {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                return Err(format!(
                    "failed to kill child: {kill_error}; initial poll error: {initial_poll_error:?}"
                ));
            }
            Err(poll_error) => {
                return Err(format!(
                    "failed to kill child: {kill_error}; initial poll error: {initial_poll_error:?}; subsequent poll failed: {poll_error}"
                ));
            }
        }
    }

    let deadline = Instant::now() + CHILD_REAP_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(format!("failed to poll killed child: {error}")),
        }
    }
    Err(format!(
        "child was not reaped within {CHILD_REAP_TIMEOUT:?} after kill"
    ))
}

fn integration_test_shell() -> PathBuf {
    std::env::var_os("MULT_TEST_SHELL")
        .map(PathBuf::from)
        .or_else(|| {
            let path = PathBuf::from("/bin/sh");
            path.exists().then_some(path)
        })
        .or_else(|| std::env::var_os("SHELL").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("sh"))
}

fn unique_socket_path() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mult-pty-integration-{}-{unique}.sock",
        std::process::id()
    ))
}
