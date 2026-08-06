//! PTY session lifecycle: restoring persisted sessions, starting terminals,
//! keeping panes sized to the visible area, and applying drained PTY events.

use std::fs;

use ratatui::layout::Rect;

use crate::{
    app::{App, NoticeLevel, NoticeSource},
    config::Config,
    model::{self, ChatStatus, PtyKey, TerminalLaunch},
    pty::{AttachExistingResult, PtyDimensions, PtyEvent, PtyRuntime, PtySpawn},
    ui,
};

use super::agent_status::{
    agent_session_metadata, chat_agent_kind, mult_agent_status_path, reconcile_agent_status,
};

pub(super) fn register_project_session_identities(app: &App, pty_runtime: &mut PtyRuntime) {
    for workspace in &app.project.workspaces {
        for chat in &workspace.chats {
            let key = PtyKey::ChatAgent(chat.id);
            if let Some(identity) = app.project.session_identity(key) {
                let _ = pty_runtime.register_session_identity(key, identity);
            }
        }
        for terminal in &workspace.terminals {
            let key = PtyKey::Terminal(terminal.id);
            if let Some(identity) = app.project.session_identity(key) {
                let _ = pty_runtime.register_session_identity(key, identity);
            }
        }
    }
}

pub(super) fn restore_persisted_sessions(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    register_project_session_identities(app, pty_runtime);
    let terminals = app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| {
            // Persisted *intent*: the terminals the user meant to have
            // running. Whether a pane is actually live is the daemon's answer,
            // which the `attach_existing` below asks for (F16). A `Command`
            // whose pane is gone is still never re-executed (C1).
            workspace.terminals.iter().filter_map(|terminal| {
                terminal.restore_on_launch.then_some((
                    workspace.id,
                    terminal.id,
                    terminal.name.clone(),
                    matches!(terminal.launch, TerminalLaunch::Command(_)),
                ))
            })
        })
        .collect::<Vec<_>>();

    for (workspace, terminal, name, is_command) in terminals {
        let key = PtyKey::Terminal(terminal);
        let size = terminal_dimensions(app, frame_area);
        match pty_runtime.attach_existing(key, size) {
            Ok(AttachExistingResult::Attached) => app.record_terminal_started(terminal),
            Ok(AttachExistingResult::Missing) => {
                app.record_terminal_stopped(terminal);
                if is_command {
                    app.mark_terminal_recoverable(terminal);
                    pty_runtime.append_terminal_system_line(
                        key,
                        format!(
                            "command terminal `{name}` was not relaunched because its daemon session is unavailable; type or use Start selected PTY to start it deliberately"
                        ),
                    );
                } else {
                    // Preserve existing shell restoration behavior. The strict
                    // no-relaunch rule applies to configured command terminals.
                    start_terminal(app, pty_runtime, config, frame_area, workspace, terminal);
                }
            }
            Err(error) if is_command => {
                app.record_terminal_stopped(terminal);
                app.mark_terminal_recoverable(terminal);
                pty_runtime.append_terminal_system_line(
                    key,
                    format!("failed to restore terminal `{name}` without relaunching it: {error}"),
                );
            }
            Err(_) => {
                app.record_terminal_stopped(terminal);
                start_terminal(app, pty_runtime, config, frame_area, workspace, terminal);
            }
        }
    }

    let chats = app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.chats.iter().filter_map(|chat| {
                app.project
                    .active_agent_generation(chat.id)
                    .map(|generation| (workspace.id, chat.id, chat.agent, generation))
            })
        })
        .collect::<Vec<_>>();

    for (_workspace, chat, agent, generation) in chats {
        let key = PtyKey::ChatAgent(chat);
        let metadata = agent_session_metadata(chat, agent, generation);
        if let Err(error) = pty_runtime.register_agent_session(key, metadata) {
            app.mark_chat_status_by_id(chat, ChatStatus::Failed);
            pty_runtime.append_terminal_system_line(
                key,
                format!("failed to restore agent generation metadata: {error}"),
            );
            continue;
        }
        let size = chat_agent_dimensions(app, frame_area);
        match pty_runtime.attach_existing(key, size) {
            Ok(AttachExistingResult::Attached) => {
                reconcile_agent_status(app, pty_runtime, chat, agent, generation);
            }
            Ok(AttachExistingResult::Missing) => {
                let recovered_final =
                    reconcile_agent_status(app, pty_runtime, chat, agent, generation);
                if !recovered_final {
                    app.mark_chat_status_by_id(chat, ChatStatus::Failed);
                }
                app.clear_agent_generation(chat, generation);
                pty_runtime.append_terminal_system_line(
                    key,
                    "agent session is unavailable; it was not relaunched during restoration",
                );
            }
            Err(error) => {
                app.mark_chat_status_by_id(chat, ChatStatus::Failed);
                pty_runtime.append_terminal_system_line(
                    key,
                    format!("failed to restore agent without relaunching it: {error}"),
                );
            }
        }
    }
}

fn start_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    let Some((workspace_id, terminal_id)) = app.selected_terminal_id() else {
        return;
    };

    start_terminal(
        app,
        pty_runtime,
        config,
        frame_area,
        workspace_id,
        terminal_id,
    );
}

pub(super) fn start_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    _config: &Config,
    frame_area: Rect,
    workspace_id: model::WorkspaceId,
    terminal_id: model::TerminalId,
) -> bool {
    let key = PtyKey::Terminal(terminal_id);
    if pty_runtime.is_running(key) {
        pty_runtime.append_terminal_system_line(key, "PTY already running");
        return true;
    }

    let Some(workspace) = app.project.workspace(workspace_id) else {
        return false;
    };
    let Some(terminal) = workspace
        .terminals
        .iter()
        .find(|terminal| terminal.id == terminal_id)
    else {
        return false;
    };

    let terminal_name = terminal.name.clone();
    let Some(identity) = app.project.session_identity(key) else {
        pty_runtime.append_terminal_system_line(key, "durable terminal identity is missing");
        return false;
    };
    if let Err(error) = pty_runtime.register_session_identity(key, identity) {
        pty_runtime.append_terminal_system_line(
            key,
            format!("failed to register durable terminal identity: {error}"),
        );
        return false;
    }
    let mut spawn = match &terminal.launch {
        TerminalLaunch::Shell => {
            PtySpawn::shell(key, workspace.cwd.clone(), workspace.environment.clone())
        }
        TerminalLaunch::Command(command) => PtySpawn::command_line(
            key,
            command.clone(),
            workspace.cwd.clone(),
            workspace.environment.clone(),
        ),
    };
    spawn.size = terminal_dimensions(app, frame_area);

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.record_terminal_started(terminal_id);
            true
        }
        Err(error) => {
            pty_runtime.append_terminal_system_line(
                key,
                format!("failed to start terminal `{terminal_name}`: {error}"),
            );
            app.record_terminal_stopped(terminal_id);
            false
        }
    }
}

pub(super) fn auto_start_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) -> bool {
    if !config.auto_start_terminals || app.is_prompt_active() {
        return false;
    }

    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return false;
    };
    if app.terminal_requires_recovery(terminal_id) {
        return false;
    }
    let key = PtyKey::Terminal(terminal_id);
    if pty_runtime.is_running(key) || !pty_runtime.terminal_output_is_blank(key) {
        return false;
    }

    start_selected_terminal(app, pty_runtime, config, frame_area);
    true
}

pub(super) fn start_or_focus_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return;
    };
    let key = PtyKey::Terminal(terminal_id);

    if !pty_runtime.is_running(key) {
        start_selected_terminal(app, pty_runtime, config, frame_area);
    }

    if pty_runtime.is_running(key) {
        app.begin_terminal_input();
    }
}

pub(super) fn resize_visible_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    _config: &Config,
    frame_area: Rect,
) -> bool {
    let Some((terminal_id, area)) = ui::selected_terminal_output_area(app, frame_area) else {
        return false;
    };
    let size = pty_dimensions_from_area(area);
    let key = PtyKey::Terminal(terminal_id);
    resize_if_changed(pty_runtime, key, size)
}

pub(super) fn resize_visible_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    _config: &Config,
    frame_area: Rect,
) -> bool {
    let Some((chat_id, area)) = ui::selected_chat_agent_output_area(app, frame_area) else {
        return false;
    };
    let terminal_id = PtyKey::ChatAgent(chat_id);
    let size = pty_dimensions_from_area(area);
    resize_if_changed(pty_runtime, terminal_id, size)
}

/// Resize `terminal` only when the size actually differs (D1).
///
/// Both callers run on every ~16 ms tick, and a `Resize` is not free at either
/// end: the client serializes and writes a message to the socket, and the
/// daemon takes the pane lock, takes the master lock and issues a
/// `TIOCSWINSZ`. Unconditionally resizing to the size the pane already has cost
/// ~125 writes/s per site at complete idle and changed nothing.
fn resize_if_changed(pty_runtime: &mut PtyRuntime, terminal: PtyKey, size: PtyDimensions) -> bool {
    if !pty_dimensions_changed(pty_runtime, terminal, size) {
        return false;
    }
    let _ = pty_runtime.resize(terminal, size);
    true
}

/// Whether resizing `terminal` to `size` would actually change its parser
/// dimensions (and therefore the rendered output). A terminal with no parser
/// yet is treated as changed so the freshly sized screen gets drawn.
fn pty_dimensions_changed(pty_runtime: &PtyRuntime, terminal: PtyKey, size: PtyDimensions) -> bool {
    match pty_runtime.parser(terminal) {
        Some(parser) => parser.screen().size() != (size.rows, size.cols),
        None => true,
    }
}

fn terminal_dimensions(app: &App, frame_area: Rect) -> PtyDimensions {
    pty_dimensions_from_area(ui::terminal_output_area_for(app, frame_area))
}

pub(super) fn chat_agent_dimensions(app: &App, frame_area: Rect) -> PtyDimensions {
    pty_dimensions_from_area(ui::chat_agent_output_area_for(app, frame_area))
}

fn pty_dimensions_from_area(area: Rect) -> PtyDimensions {
    PtyDimensions {
        rows: area.height.max(1),
        cols: area.width.max(1),
    }
}

pub(super) fn drain_pty_events(app: &mut App, pty_runtime: &mut PtyRuntime) -> bool {
    let mut changed = false;
    for event in pty_runtime.drain_events() {
        changed = true;
        apply_pty_event(app, pty_runtime, event);
    }
    // `drain_events` stops at a per-frame budget, so a busy pane can leave
    // traffic (or queued re-attachments) behind. Asking for a redraw keeps the
    // loop coming back for the rest instead of parking on a stale frame.
    changed || pty_runtime.has_pending_work()
}

fn apply_pty_event(app: &mut App, pty_runtime: &mut PtyRuntime, event: PtyEvent) {
    {
        match event {
            PtyEvent::Scrollback { .. } | PtyEvent::Output { .. } => {}
            // Truncation is metadata, not terminal output. Injecting a notice
            // into the parser here would place client text after replay/live
            // bytes and corrupt the daemon's exact byte ordering.
            PtyEvent::ReplayTruncated { .. } => {}
            PtyEvent::TakenOver { terminal } => {
                match terminal {
                    PtyKey::ChatAgent(chat_id) => {
                        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
                    }
                    PtyKey::Terminal(terminal_id) => {
                        app.record_terminal_stopped(terminal_id);
                    }
                }
                if app.pty_input_target() == Some(terminal) {
                    app.end_pty_input();
                }
                pty_runtime.append_terminal_system_line(
                    terminal,
                    "PTY attachment was taken over by another client",
                );
            }
            PtyEvent::Exited { terminal, status } => match terminal {
                PtyKey::ChatAgent(chat_id) => {
                    let chat_status = if status.code == 0 {
                        ChatStatus::Done
                    } else {
                        ChatStatus::Failed
                    };
                    let agent = chat_agent_kind(app, chat_id);
                    app.mark_chat_status_by_id(chat_id, chat_status);
                    if let (Some(identity), Some(generation)) = (
                        app.project.session_identity(terminal),
                        app.project.active_agent_generation(chat_id),
                    ) {
                        app.clear_agent_generation(chat_id, generation);
                        let _ = fs::remove_file(mult_agent_status_path(identity, generation));
                    }
                    if app.pty_input_target() == Some(terminal) {
                        app.end_pty_input();
                    }
                    let exit_message =
                        format!("{} agent exited: {}", agent.display_name(), status.label());
                    pty_runtime.append_terminal_system_line(terminal, exit_message.as_str());
                }
                PtyKey::Terminal(terminal_id) => {
                    app.record_terminal_stopped(terminal_id);
                    if app.terminal_input_target() == Some(terminal_id) {
                        app.end_pty_input();
                    }
                    let exit_message = format!("PTY exited: {}", status.label());
                    pty_runtime.append_terminal_system_line(terminal, exit_message.as_str());
                }
            },
            PtyEvent::Error { terminal, message } => {
                pty_runtime.append_terminal_system_line(terminal, message.as_str());
            }
            // No pane owns this, so there is no pane to write it into: a
            // missing or protocol-incompatible daemon otherwise left the user
            // with an inert UI and the explanation queued against a terminal
            // id that cannot exist (E2/B8).
            PtyEvent::ConnectionError { message } => {
                app.push_notice(NoticeLevel::Error, NoticeSource::Report, message);
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use mult_protocol::{ClientMessage, SessionId};

    use super::*;
    use crate::app::NavItem;
    use crate::pty::SpawnPolicy;
    use crate::runtime::test_support::*;
    use crate::storage;
    use mult_protocol::shell::quote_argument;
    use std::time::Duration;

    /// D1: both resize sites ran on every ~16 ms tick and called `resize`
    /// unconditionally, so an idle session wrote a `Resize` to the socket ~125
    /// times a second — each one a pane lock, a master lock and a `TIOCSWINSZ`
    /// in the daemon for a size that had not changed.
    #[test]
    fn a_visible_pane_is_resized_only_when_its_size_changed() {
        let (mut app, _, terminal) = running_command_app("echo recorded".to_string());
        let (mut runtime, observed, server, socket_path) = recording_attached_runtime(terminal);
        let config = Config::default();
        let area = Rect::new(0, 0, 120, 40);

        restore_persisted_sessions(&mut app, &mut runtime, &config, area);
        // Settle the pane at the visible size, whatever the attach reported.
        resize_visible_terminal(&mut app, &mut runtime, &config, area);

        for _ in 0..8 {
            assert!(
                !resize_visible_terminal(&mut app, &mut runtime, &config, area),
                "an unchanged size is not a redraw reason either"
            );
        }
        // A genuine resize must still reach the daemon.
        assert!(resize_visible_terminal(
            &mut app,
            &mut runtime,
            &config,
            Rect::new(0, 0, 100, 30)
        ));

        drop(runtime);
        server.join().expect("recording server exits");
        let resizes = observed
            .into_iter()
            .filter_map(|message| match message {
                ClientMessage::Resize { rows, cols, .. } => Some((rows, cols)),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            !resizes.is_empty(),
            "a genuine resize must still reach the daemon"
        );
        // The eight idle ticks are the point: before the fix each one wrote the
        // size the pane already had.
        let distinct = resizes.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(
            distinct.len(),
            resizes.len(),
            "no size may be written twice: {resizes:?}"
        );
        assert!(
            resizes.len() <= 2,
            "at most one write per size: {resizes:?}"
        );
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn restoration_attaches_to_an_existing_command_session_without_creating_it() {
        let (mut app, _, terminal) = running_command_app("echo restored".to_string());
        let (mut runtime, observed, server, socket_path) =
            connected_restoration_runtime(terminal, RestorationReply::Attached);

        restore_persisted_sessions(
            &mut app,
            &mut runtime,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        );

        assert!(matches!(
            observed
                .recv_timeout(Duration::from_secs(2))
                .expect("observe restoration request"),
            ClientMessage::Attach { session, .. } if session == SessionId(terminal.0)
        ));
        assert!(runtime.is_running(PtyKey::Terminal(terminal)));
        assert!(
            app.project
                .terminal_mut_by_id(terminal)
                .unwrap()
                .restore_on_launch
        );
        assert!(!app.terminal_requires_recovery(terminal));
        server.join().expect("restoration server exits");
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn missing_persisted_command_session_is_stopped_without_command_execution() {
        let side_effect = unique_status_path("must-not-run");
        let _ = fs::remove_file(&side_effect);
        let command = format!(
            "printf launched > {}",
            quote_argument(&side_effect.display().to_string())
        );
        let (mut app, workspace, terminal) = running_command_app(command);
        let (mut runtime, observed, server, socket_path) =
            connected_restoration_runtime(terminal, RestorationReply::Missing);

        restore_persisted_sessions(
            &mut app,
            &mut runtime,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        );

        assert!(matches!(
            observed
                .recv_timeout(Duration::from_secs(2))
                .expect("observe restoration request"),
            ClientMessage::Attach { .. }
        ));
        assert!(
            !side_effect.exists(),
            "restoration must not execute the command"
        );
        assert!(
            !app.project
                .terminal(workspace, terminal)
                .unwrap()
                .restore_on_launch
        );
        assert!(app.terminal_requires_recovery(terminal));

        // Saving the cleared restore intent and loading it again remains conservative:
        // a blank pane still cannot auto-start until deliberate user input.
        let mut reloaded = App::new(app.project.clone());
        reloaded.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        let offline_socket = unique_status_path("offline-restore").with_extension("sock");
        let mut offline = PtyRuntime::with_socket_path(offline_socket, SpawnPolicy::Autospawn);
        assert!(!auto_start_selected_terminal(
            &mut reloaded,
            &mut offline,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        ));
        assert!(!side_effect.exists());

        server.join().expect("restoration server exits");
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn migrated_v1_command_restoration_uses_generated_identity_and_only_attach() {
        use std::os::unix::fs::DirBuilderExt;

        let root = unique_status_path("v1-migration").with_extension("");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .unwrap();
        let state_path = root.join("state.json");
        let side_effect = root.join("must-not-run");
        let command = format!(
            "printf launched > {}",
            quote_argument(&side_effect.display().to_string())
        );
        let v1 = serde_json::json!({
            "version": 1,
            "next_workspace_id": 2,
            "next_chat_id": 1,
            "next_terminal_id": 2,
            "workspaces": [{
                "id": 1,
                "name": "migrated",
                "cwd": null,
                "environment": {},
                "chats": [],
                "terminals": [{
                    "id": 1,
                    "name": "command",
                    "status": "Running",
                    "launch": { "kind": "command", "command": command }
                }]
            }]
        });
        fs::write(&state_path, serde_json::to_vec_pretty(&v1).unwrap()).unwrap();
        let store = storage::StateStore::acquire(
            storage::StatePaths::from_explicit_path(state_path.clone()).unwrap(),
        )
        .unwrap();
        let loaded = store.load_or_default().unwrap();
        assert!(loaded.needs_save);
        let terminal = loaded.state.workspaces[0].terminals[0].id;
        let expected_identity = loaded
            .state
            .session_identity(PtyKey::Terminal(terminal))
            .unwrap();
        store.save(&loaded.state).unwrap();
        let mut app = App::new(loaded.state);
        let (mut runtime, observed, server, socket_path) =
            connected_restoration_runtime(terminal, RestorationReply::Missing);

        restore_persisted_sessions(
            &mut app,
            &mut runtime,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        );

        let request = observed
            .recv_timeout(Duration::from_secs(2))
            .expect("migration restoration sends one request");
        let ClientMessage::Attach { identity, .. } = request else {
            panic!("migration restoration must send Attach only");
        };
        assert_eq!(
            identity.namespace.into_bytes(),
            expected_identity.namespace.as_bytes()
        );
        assert_eq!(
            identity.token.into_bytes(),
            expected_identity.token.as_bytes()
        );
        assert!(!side_effect.exists());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(state_path).unwrap()).unwrap()
                ["version"],
            model::STATE_VERSION
        );

        server.join().unwrap();
        let _ = fs::remove_file(socket_path);
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unreachable_daemon_marks_running_command_recoverable_without_execution() {
        let side_effect = unique_status_path("unreachable-must-not-run");
        let _ = fs::remove_file(&side_effect);
        let command = format!(
            "printf launched > {}",
            quote_argument(&side_effect.display().to_string())
        );
        let (mut app, workspace, terminal) = running_command_app(command);
        let socket = unique_status_path("unreachable-daemon").with_extension("sock");
        let mut runtime = PtyRuntime::with_socket_path(socket, SpawnPolicy::Autospawn);

        restore_persisted_sessions(
            &mut app,
            &mut runtime,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        );

        assert!(
            !app.project
                .terminal(workspace, terminal)
                .unwrap()
                .restore_on_launch
        );
        assert!(app.terminal_requires_recovery(terminal));
        assert!(!side_effect.exists());
    }

    #[test]
    fn restored_terminal_dimensions_use_visible_main_width_even_when_not_selected() {
        let app = App::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        assert_eq!(
            terminal_dimensions(&app, frame_area),
            PtyDimensions { rows: 40, cols: 86 }
        );
    }

    // ---- E2 / B8: connection failures reach the user ----------------------

    #[test]
    fn a_connection_wide_failure_is_reported_on_the_status_surface() {
        // A daemon that will not connect used to queue its (good) diagnostic
        // against `PtyKey::Terminal(TerminalId(0))`, a pane that cannot exist,
        // so the user saw an inert UI and no explanation at all.
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();

        apply_pty_event(
            &mut app,
            &mut pty_runtime,
            PtyEvent::ConnectionError {
                message: "protocol version 9 is not supported".to_string(),
            },
        );

        assert_eq!(app.notices().len(), 1);
        assert_eq!(app.notices()[0].level(), NoticeLevel::Error);
        assert!(app.notices()[0].text().contains("protocol version 9"));
    }
}
