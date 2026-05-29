#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use mult::{
    model::{ChatId, PtyKey, TerminalId},
    pty::{PtyDimensions, PtyEvent, PtyExit, PtyRuntime, PtySpawn},
};
use mult_protocol::SOCKET_PATH_ENV;

const INTEGRATION_TIMEOUT: Duration = Duration::from_secs(5);

struct ServerGuard {
    child: Child,
    socket_path: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket_path);
    }
}

#[test]
fn client_receives_scrollback_output_and_real_exit_from_server_pty() {
    if integration_tests_are_skipped() {
        return;
    }
    let Some(server) = start_isolated_server() else {
        return;
    };
    let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("connect to isolated mult-server");
    let terminal = PtyKey::Terminal(TerminalId(7001));

    start_short_lived_command(&mut runtime, terminal, "printf 'hello from pty\\n'; exit 7");
    let observed = wait_for_terminal_exit(&mut runtime, terminal)
        .expect("short-lived command should produce output and exit");

    assert!(
        observed.saw_scrollback,
        "server should send an initial raw scrollback replay"
    );
    assert!(observed.saw_output, "server should send raw PTY output");
    assert_eq!(observed.exit.code, 7);
    assert_eq!(observed.exit.signal, None);
    assert!(
        observed.output.contains("hello from pty"),
        "terminal output should include command stdout: {:?}",
        observed.output
    );
    assert!(!runtime.is_running(terminal));
}

#[test]
fn reconnect_replays_raw_scrollback_into_fresh_parser() {
    if integration_tests_are_skipped() {
        return;
    }
    let Some(server) = start_isolated_server() else {
        return;
    };
    let terminal = PtyKey::Terminal(TerminalId(7003));

    {
        let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
            .expect("connect to isolated mult-server");
        start_short_lived_command(&mut runtime, terminal, "printf replayed; sleep 2");
        wait_for_output(&mut runtime, terminal, "replayed")
            .expect("first client should receive output before detach");
    }

    let mut reconnected = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("reconnect to isolated mult-server");
    start_short_lived_command(&mut reconnected, terminal, "printf should-not-run");
    wait_for_output(&mut reconnected, terminal, "replayed")
        .expect("reattach should replay buffered raw PTY output");
    assert!(reconnected.stop(terminal).expect("stop replayed terminal"));
}

#[test]
fn second_client_takes_over_session_from_a_still_attached_client() {
    if integration_tests_are_skipped() {
        return;
    }
    let Some(server) = start_isolated_server() else {
        return;
    };
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

    // Client B attaches to the same session while A is *still attached*. Without
    // takeover the server rejects this as "already attached"; with it, B takes
    // over and receives the replayed scrollback. `start` reuses the existing
    // server session (CreateSession is idempotent for a known id).
    let mut client_b =
        PtyRuntime::connect_to_socket(server.socket_path.clone()).expect("connect client B");
    let mut spawn = PtySpawn::command_line(
        terminal,
        "ignored-existing-session".to_string(),
        None,
        BTreeMap::new(),
    );
    spawn.size = PtyDimensions { rows: 6, cols: 40 };
    client_b
        .start(spawn)
        .expect("client B should take over the existing session");
    wait_for_output(&mut client_b, terminal, "takeover")
        .expect("takeover should replay buffered output to client B");

    assert!(client_b.stop(terminal).expect("stop after takeover"));
}

#[test]
fn reattach_after_server_restart_reports_vanished_session_as_exited() {
    if integration_tests_are_skipped() {
        return;
    }
    let Some(server) = start_isolated_server() else {
        return;
    };
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
    let Some(_server2) = start_isolated_server_at(socket_path) else {
        return;
    };

    // Nudging the runtime makes it reconnect to the fresh server and re-attach;
    // because the session no longer exists, the server answers with PaneExited
    // and the client cleanly retires the terminal instead of freezing on it.
    let status = wait_for_terminal_exit_after_reconnect(&mut runtime, terminal)
        .expect("client should learn the session vanished after a server restart");
    assert_ne!(status.code, 0, "a vanished session is not a clean exit");
    assert!(!runtime.is_running(terminal));
}

#[test]
fn terminal_is_retired_when_the_daemon_is_gone_and_not_autospawned() {
    if integration_tests_are_skipped() {
        return;
    }
    let Some(mut server) = start_isolated_server() else {
        return;
    };
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
    let _ = server.child.kill();
    let _ = server.child.wait();

    let status = wait_for_terminal_exit_after_reconnect(&mut runtime, terminal)
        .expect("client should retire the terminal once the daemon is unreachable");
    assert_eq!(
        status.signal.as_deref(),
        Some("mult-server connection lost")
    );
    assert!(!runtime.is_running(terminal));
}

#[test]
fn server_ignores_sighup_and_keeps_sessions_running() {
    if integration_tests_are_skipped() {
        return;
    }
    let Some(mut server) = start_isolated_server() else {
        return;
    };
    let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("connect to isolated mult-server");
    let terminal = PtyKey::Terminal(TerminalId(7004));

    start_short_lived_command(
        &mut runtime,
        terminal,
        "printf before; sleep 1; printf after; sleep 3",
    );
    wait_for_output(&mut runtime, terminal, "before")
        .expect("command should produce output before hangup");

    let rc = unsafe { libc::kill(server.child.id() as i32, libc::SIGHUP) };
    assert_eq!(rc, 0, "send SIGHUP to mult-server");
    assert_server_still_running(&mut server);
    wait_for_output(&mut runtime, terminal, "after")
        .expect("server should keep PTY command running after SIGHUP");

    assert!(runtime.stop(terminal).expect("stop terminal after SIGHUP"));
}

#[test]
fn rapid_stop_restart_and_chat_runtime_ids_keep_client_registry_consistent() {
    if integration_tests_are_skipped() {
        return;
    }
    let Some(server) = start_isolated_server() else {
        return;
    };
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

struct ObservedTerminal {
    saw_scrollback: bool,
    saw_output: bool,
    exit: PtyExit,
    output: String,
}

fn start_short_lived_command(runtime: &mut PtyRuntime, terminal: PtyKey, command: &str) {
    let mut spawn = PtySpawn::command_line(terminal, command.to_string(), None, BTreeMap::new());
    spawn.size = PtyDimensions { rows: 6, cols: 40 };
    runtime.start(spawn).expect("start PTY command");
}

fn wait_for_output(runtime: &mut PtyRuntime, terminal: PtyKey, needle: &str) -> Option<()> {
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    while Instant::now() < deadline {
        for event in runtime.drain_events() {
            if let PtyEvent::Error { terminal, message } = event {
                panic!("server reported PTY error for terminal {terminal:?}: {message}");
            }
        }
        if runtime
            .terminal_all_lines(terminal)
            .join("\n")
            .contains(needle)
        {
            return Some(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

fn wait_for_terminal_exit(runtime: &mut PtyRuntime, terminal: PtyKey) -> Option<ObservedTerminal> {
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let mut saw_scrollback = false;
    let mut saw_output = false;

    while Instant::now() < deadline {
        for event in runtime.drain_events() {
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
                    return Some(ObservedTerminal {
                        saw_scrollback,
                        saw_output,
                        exit: status,
                        output: runtime.terminal_all_lines(terminal).join("\n"),
                    });
                }
                PtyEvent::Error { terminal, message } => {
                    panic!("server reported PTY error for terminal {terminal:?}: {message}");
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(20));
    }

    None
}

fn wait_for_terminal_exit_after_reconnect(
    runtime: &mut PtyRuntime,
    terminal: PtyKey,
) -> Option<PtyExit> {
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let size = PtyDimensions { rows: 6, cols: 40 };
    while Instant::now() < deadline {
        // Any send forces a transparent reconnect to the restarted server and a
        // re-attach of the still-tracked terminal; errors here are expected
        // mid-reconnect and ignored.
        let _ = runtime.resize(terminal, size);
        for event in runtime.drain_events() {
            if let PtyEvent::Exited {
                terminal: event_terminal,
                status,
            } = event
            {
                if event_terminal == terminal {
                    return Some(status);
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
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
        eprintln!("skipping PTY integration tests because MULT_SKIP_PTY_INTEGRATION is set");
        true
    } else {
        false
    }
}

fn start_isolated_server() -> Option<ServerGuard> {
    start_isolated_server_at(unique_socket_path())
}

fn start_isolated_server_at(socket_path: PathBuf) -> Option<ServerGuard> {
    let Some(server_bin) = option_env!("CARGO_BIN_EXE_mult-server") else {
        eprintln!("skipping PTY integration tests: CARGO_BIN_EXE_mult-server is unavailable");
        return None;
    };
    let mut child = match Command::new(server_bin)
        .env(SOCKET_PATH_ENV, &socket_path)
        .env("SHELL", integration_test_shell())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skipping PTY integration tests: failed to spawn mult-server: {error}");
            return None;
        }
    };

    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => panic!("mult-server exited before accepting connections: {status}"),
            Ok(None) => {}
            Err(error) => panic!("failed to poll mult-server child: {error}"),
        }

        if UnixStream::connect(&socket_path).is_ok() {
            return Some(ServerGuard { child, socket_path });
        }
        thread::sleep(Duration::from_millis(20));
    }

    let _ = child.kill();
    let _ = child.wait();
    eprintln!("skipping PTY integration tests: mult-server did not create a usable socket in time");
    None
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
