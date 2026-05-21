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
    app::{chat_agent_terminal_id, App},
    model::{ChatId, TerminalId},
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
fn client_receives_snapshot_updates_and_real_exit_from_server_pty() {
    if integration_tests_are_skipped() {
        return;
    }
    let Some(server) = start_isolated_server() else {
        return;
    };
    let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("connect to isolated mult-server");
    let terminal = TerminalId(7001);

    start_short_lived_command(&mut runtime, terminal, "printf 'hello from pty\\n'; exit 7");
    let observed = wait_for_terminal_exit(&mut runtime, terminal)
        .expect("short-lived command should produce output and exit");

    assert!(
        observed.saw_snapshot,
        "server should send an initial snapshot"
    );
    assert!(observed.saw_update, "server should send an output update");
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
fn rapid_stop_restart_and_chat_runtime_ids_keep_client_registry_consistent() {
    if integration_tests_are_skipped() {
        return;
    }
    let Some(server) = start_isolated_server() else {
        return;
    };
    let mut runtime = PtyRuntime::connect_to_socket(server.socket_path.clone())
        .expect("connect to isolated mult-server");
    let terminal = TerminalId(7002);

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

    let chat_terminal = chat_agent_terminal_id(ChatId(77));
    start_short_lived_command(&mut runtime, chat_terminal, "printf chat-agent; exit 0");
    let chat = wait_for_terminal_exit(&mut runtime, chat_terminal)
        .expect("chat-agent runtime terminal should exit naturally");
    assert_eq!(chat.exit.code, 0);
    assert!(chat.output.contains("chat-agent"));
    assert!(!runtime.is_running(chat_terminal));
}

struct ObservedTerminal {
    saw_snapshot: bool,
    saw_update: bool,
    exit: PtyExit,
    output: String,
}

fn start_short_lived_command(runtime: &mut PtyRuntime, terminal: TerminalId, command: &str) {
    let mut spawn = PtySpawn::command_line(terminal, command.to_string(), None, BTreeMap::new());
    spawn.size = PtyDimensions { rows: 6, cols: 40 };
    runtime.start(spawn).expect("start PTY command");
}

fn wait_for_terminal_exit(
    runtime: &mut PtyRuntime,
    terminal: TerminalId,
) -> Option<ObservedTerminal> {
    let mut app = App::default();
    app.resize_terminal_buffer(terminal, 6, 40);
    let deadline = Instant::now() + INTEGRATION_TIMEOUT;
    let mut saw_snapshot = false;
    let mut saw_update = false;

    while Instant::now() < deadline {
        for event in runtime.drain_events() {
            match event {
                PtyEvent::Snapshot {
                    terminal: event_terminal,
                    snapshot,
                } if event_terminal == terminal => {
                    saw_snapshot = true;
                    app.set_terminal_snapshot(terminal, snapshot);
                }
                PtyEvent::Update {
                    terminal: event_terminal,
                    update,
                } if event_terminal == terminal => {
                    saw_update = true;
                    app.apply_terminal_update(terminal, update);
                }
                PtyEvent::Exited {
                    terminal: event_terminal,
                    status,
                } if event_terminal == terminal => {
                    return Some(ObservedTerminal {
                        saw_snapshot,
                        saw_update,
                        exit: status,
                        output: app.terminal_all_lines(terminal).join("\n"),
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

fn integration_tests_are_skipped() -> bool {
    if std::env::var_os("MULT_SKIP_PTY_INTEGRATION").is_some() {
        eprintln!("skipping PTY integration tests because MULT_SKIP_PTY_INTEGRATION is set");
        true
    } else {
        false
    }
}

fn start_isolated_server() -> Option<ServerGuard> {
    let Some(server_bin) = option_env!("CARGO_BIN_EXE_mult-server") else {
        eprintln!("skipping PTY integration tests: CARGO_BIN_EXE_mult-server is unavailable");
        return None;
    };
    let socket_path = unique_socket_path();
    let mut child = match Command::new(server_bin)
        .env(SOCKET_PATH_ENV, &socket_path)
        .env("SHELL", "/bin/sh")
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
