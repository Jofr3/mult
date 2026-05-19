pub mod agent;
mod app;
mod config;
mod model;
mod pty;
mod storage;
mod ui;

use std::{io, time::Duration};

use agent::{AgentBackend, AgentEvent, NoopAgentBackend, ProcessAgentBackend, ProcessAgentCommand};
use app::{chat_agent_terminal_id, chat_id_from_agent_terminal_id, App, Mode};
use config::Config;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use model::{ChatStatus, TerminalId, TerminalLaunch};
use pty::{PtyDimensions, PtyEvent, PtyRuntime, PtySpawn};
use ratatui::{layout::Rect, DefaultTerminal};

fn main() -> io::Result<()> {
    let project = storage::load_or_default()?;
    let config = config::load_or_default()?;
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, App::new(project), config);
    ratatui::restore();
    result
}

const AGENT_CMD_ENV: &str = "MULT_AGENT_CMD";
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(8);
const READY_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(0);

enum RuntimeAgentBackend {
    Noop(NoopAgentBackend),
    Process(ProcessAgentBackend),
}

impl RuntimeAgentBackend {
    fn from_env() -> Self {
        std::env::var(AGENT_CMD_ENV)
            .ok()
            .and_then(|raw| parse_process_agent_command(&raw))
            .map(ProcessAgentBackend::new)
            .map(Self::Process)
            .unwrap_or_else(|| Self::Noop(NoopAgentBackend))
    }
}

impl AgentBackend for RuntimeAgentBackend {
    fn send_prompt(&mut self, prompt: agent::AgentPrompt) -> io::Result<()> {
        match self {
            Self::Noop(backend) => backend.send_prompt(prompt),
            Self::Process(backend) => backend.send_prompt(prompt),
        }
    }

    fn drain_events(&mut self) -> Vec<AgentEvent> {
        match self {
            Self::Noop(backend) => backend.drain_events(),
            Self::Process(backend) => backend.drain_events(),
        }
    }
}

fn parse_process_agent_command(raw: &str) -> Option<ProcessAgentCommand> {
    let mut parts = raw.split_whitespace().map(ToOwned::to_owned);
    let program = parts.next()?;
    if program.is_empty() {
        return None;
    }

    Some(ProcessAgentCommand::with_args(program, parts))
}

fn run(terminal: &mut DefaultTerminal, mut app: App, config: Config) -> io::Result<()> {
    let mut pty_runtime = PtyRuntime::default();
    let mut agent_backend = RuntimeAgentBackend::from_env();
    let size = terminal.size()?;
    let mut frame_area = Rect::new(0, 0, size.width, size.height);

    while !app.should_quit {
        drain_pty_events(&mut app, &mut pty_runtime);
        drain_agent_events(&mut app, &mut agent_backend);
        save_if_dirty(&mut app)?;
        resize_visible_terminal(&mut app, &mut pty_runtime, frame_area);
        resize_visible_chat_agent(&mut app, &mut pty_runtime, frame_area);
        auto_start_selected_terminal(&mut app, &mut pty_runtime, &config, frame_area);
        auto_start_selected_chat_agent(&mut app, &mut pty_runtime, &config, frame_area);
        frame_area = terminal.draw(|frame| ui::draw(frame, &app))?.area;

        if event::poll(EVENT_POLL_INTERVAL)? {
            handle_event(
                &mut app,
                &mut pty_runtime,
                &config,
                event::read()?,
                frame_area,
            );
            while !app.should_quit && event::poll(READY_EVENT_POLL_INTERVAL)? {
                handle_event(
                    &mut app,
                    &mut pty_runtime,
                    &config,
                    event::read()?,
                    frame_area,
                );
            }
            save_if_dirty(&mut app)?;
        }
    }

    save_if_dirty(&mut app)?;
    Ok(())
}

fn handle_event(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    event: Event,
    frame_area: Rect,
) {
    if let Event::Key(key) = event {
        if key.kind == KeyEventKind::Press {
            handle_key(app, pty_runtime, config, key, frame_area);
        }
    }
}

fn handle_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    frame_area: Rect,
) {
    if matches!(&app.mode, Mode::OpenWorkspace(_)) {
        handle_open_workspace_key(app, key);
    } else if matches!(&app.mode, Mode::NewTerminalCommand(_)) {
        handle_terminal_command_key(app, key);
    } else if matches!(&app.mode, Mode::ConfirmDelete(_)) {
        handle_delete_confirmation_key(app, pty_runtime, key);
    } else if matches!(
        &app.mode,
        Mode::TerminalInput { .. } | Mode::ChatAgentInput { .. }
    ) {
        handle_pty_input_key(app, pty_runtime, key);
    } else {
        handle_normal_key(app, pty_runtime, config, key, frame_area);
    }
}

fn handle_normal_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    frame_area: Rect,
) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit(),
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_previous(),
        KeyCode::Char('w') => app.add_workspace(),
        KeyCode::Char('o') => app.begin_open_workspace(),
        KeyCode::Char('c') => {
            if let Some((workspace, chat)) = app.add_chat_to_selected_workspace_and_return() {
                start_or_focus_chat_agent(
                    app,
                    pty_runtime,
                    config,
                    frame_area,
                    workspace,
                    chat,
                    true,
                );
            }
        }
        KeyCode::Char('p') => {
            start_or_focus_selected_chat_agent(app, pty_runtime, config, frame_area)
        }
        KeyCode::Char('t') => app.add_terminal_to_selected_workspace(),
        KeyCode::Char('d') => {
            app.begin_new_terminal_command();
        }
        KeyCode::Char('r') => app.rotate_selected_status(),
        KeyCode::Char('D') | KeyCode::Delete => {
            app.begin_delete_selected();
        }
        KeyCode::Char('s') => start_selected_terminal(app, pty_runtime, frame_area),
        KeyCode::Char('x') => stop_selected_pane(app, pty_runtime),
        KeyCode::Char('i') => focus_selected_pty(app, pty_runtime),
        _ => {}
    }
}

fn handle_pty_input_key(app: &mut App, pty_runtime: &mut PtyRuntime, key: KeyEvent) {
    if key.code == KeyCode::Esc {
        app.end_pty_input();
        return;
    }

    let Some(terminal_id) = app.pty_input_target() else {
        app.end_pty_input();
        return;
    };

    let bytes = key_to_pty_bytes(key);
    if bytes.is_empty() {
        return;
    }

    match pty_runtime.send_input(terminal_id, &bytes) {
        Ok(true) => {}
        Ok(false) => {
            app.append_terminal_system_line(terminal_id, "PTY is not running");
            app.end_pty_input();
        }
        Err(error) => {
            app.append_terminal_system_line(terminal_id, format!("failed to send input: {error}"));
            app.end_pty_input();
        }
    }
}

fn handle_open_workspace_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.cancel_prompt(),
        KeyCode::Enter => app.submit_open_workspace(),
        KeyCode::Backspace => app.pop_prompt_char(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_prompt(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.push_prompt_char(c);
        }
        _ => {}
    }
}

fn handle_terminal_command_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.cancel_prompt(),
        KeyCode::Enter => app.submit_new_terminal_command(),
        KeyCode::Backspace => app.pop_prompt_char(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_prompt(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.push_prompt_char(c);
        }
        _ => {}
    }
}

fn handle_delete_confirmation_key(app: &mut App, pty_runtime: &mut PtyRuntime, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') => app.cancel_prompt(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_prompt(),
        KeyCode::Enter | KeyCode::Char('y') => {
            for terminal in app.confirm_delete_selected() {
                let _ = pty_runtime.stop(terminal);
            }
        }
        _ => {}
    }
}

fn start_selected_terminal(app: &mut App, pty_runtime: &mut PtyRuntime, frame_area: Rect) {
    let Some((workspace_id, terminal_id)) = app.selected_terminal_id() else {
        return;
    };

    if pty_runtime.is_running(terminal_id) {
        app.append_terminal_system_line(terminal_id, "PTY already running");
        return;
    }

    let Some(workspace) = app.project.workspace(workspace_id) else {
        return;
    };
    let Some(terminal) = workspace
        .terminals
        .iter()
        .find(|terminal| terminal.id == terminal_id)
    else {
        return;
    };

    let mut spawn = match &terminal.launch {
        TerminalLaunch::Shell => PtySpawn::shell(
            terminal_id,
            workspace.cwd.clone(),
            workspace.environment.clone(),
        ),
        TerminalLaunch::Command(command) => PtySpawn::command_line(
            terminal_id,
            command.clone(),
            workspace.cwd.clone(),
            workspace.environment.clone(),
        ),
    };
    spawn.size = selected_terminal_dimensions(app, frame_area, terminal_id).unwrap_or_default();
    let size = spawn.size;
    app.resize_terminal_buffer(terminal_id, size.rows, size.cols);

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.mark_terminal_running(terminal_id);
        }
        Err(error) => {
            app.append_terminal_system_line(terminal_id, format!("failed to start PTY: {error}"));
        }
    }
}

fn start_or_focus_selected_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    let Some((workspace_id, chat_id)) = app.selected_chat_id() else {
        return;
    };

    start_or_focus_chat_agent(
        app,
        pty_runtime,
        config,
        frame_area,
        workspace_id,
        chat_id,
        true,
    );
}

fn start_or_focus_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
    workspace_id: crate::model::WorkspaceId,
    chat_id: crate::model::ChatId,
    focus_after_start: bool,
) {
    let terminal_id = chat_agent_terminal_id(chat_id);

    if pty_runtime.is_running(terminal_id) {
        if focus_after_start {
            app.begin_chat_agent_input();
        }
        return;
    }

    let Some(workspace) = app.project.workspace(workspace_id) else {
        return;
    };
    let command = pi_command(config);
    let mut spawn = PtySpawn::command_line(
        terminal_id,
        command.clone(),
        workspace.cwd.clone(),
        workspace.environment.clone(),
    );
    spawn.size = selected_chat_agent_dimensions(app, frame_area, chat_id).unwrap_or_default();
    let size = spawn.size;
    app.resize_terminal_buffer(terminal_id, size.rows, size.cols);

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Thinking);
            if focus_after_start {
                app.begin_chat_agent_input();
            }
        }
        Err(error) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
            app.append_terminal_system_line(
                terminal_id,
                format!("failed to start pi agent: {error}"),
            );
        }
    }
}

fn auto_start_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    if !config.auto_start_terminals || !matches!(app.mode, Mode::Normal) {
        return;
    }

    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return;
    };
    if pty_runtime.is_running(terminal_id) || !terminal_output_is_blank(app, terminal_id) {
        return;
    }

    start_selected_terminal(app, pty_runtime, frame_area);
}

fn auto_start_selected_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    if !config.auto_start_pi_agent || !matches!(app.mode, Mode::Normal) {
        return;
    }

    let Some((workspace_id, chat_id)) = app.selected_chat_id() else {
        return;
    };
    let terminal_id = chat_agent_terminal_id(chat_id);
    if pty_runtime.is_running(terminal_id) || !terminal_output_is_blank(app, terminal_id) {
        return;
    }

    start_or_focus_chat_agent(
        app,
        pty_runtime,
        config,
        frame_area,
        workspace_id,
        chat_id,
        false,
    );
}

fn terminal_output_is_blank(app: &App, terminal_id: TerminalId) -> bool {
    app.terminal_lines(terminal_id)
        .iter()
        .all(|line| line.trim().is_empty())
}

fn pi_command(config: &Config) -> String {
    let command = config.pi_agent_command.trim();
    if command.is_empty() {
        "pi".to_string()
    } else {
        command.to_string()
    }
}

fn stop_selected_terminal(app: &mut App, pty_runtime: &mut PtyRuntime) {
    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return;
    };

    match pty_runtime.stop(terminal_id) {
        Ok(true) => {
            app.mark_terminal_stopped(terminal_id);
            app.append_terminal_system_line(terminal_id, "stopped PTY shell");
        }
        Ok(false) => app.append_terminal_system_line(terminal_id, "PTY is not running"),
        Err(error) => {
            app.append_terminal_system_line(terminal_id, format!("failed to stop PTY: {error}"));
        }
    }
}

fn stop_selected_chat_agent(app: &mut App, pty_runtime: &mut PtyRuntime) {
    let Some((_, chat_id)) = app.selected_chat_id() else {
        return;
    };
    let terminal_id = chat_agent_terminal_id(chat_id);

    match pty_runtime.stop(terminal_id) {
        Ok(true) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Idle);
            app.append_terminal_system_line(terminal_id, "stopped pi agent");
        }
        Ok(false) => app.append_terminal_system_line(terminal_id, "pi agent is not running"),
        Err(error) => {
            app.append_terminal_system_line(
                terminal_id,
                format!("failed to stop pi agent: {error}"),
            );
        }
    }
}

fn stop_selected_pane(app: &mut App, pty_runtime: &mut PtyRuntime) {
    if app.selected_chat_id().is_some() {
        stop_selected_chat_agent(app, pty_runtime);
    } else {
        stop_selected_terminal(app, pty_runtime);
    }
}

fn focus_selected_terminal(app: &mut App, pty_runtime: &mut PtyRuntime) {
    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return;
    };

    if pty_runtime.is_running(terminal_id) {
        app.begin_terminal_input();
    } else {
        app.append_terminal_system_line(terminal_id, "start PTY before focusing input");
    }
}

fn focus_selected_chat_agent(app: &mut App, pty_runtime: &mut PtyRuntime) {
    let Some((_, chat_id)) = app.selected_chat_id() else {
        return;
    };
    let terminal_id = chat_agent_terminal_id(chat_id);

    if pty_runtime.is_running(terminal_id) {
        app.begin_chat_agent_input();
    } else {
        app.append_terminal_system_line(
            terminal_id,
            "start pi agent with `p` before focusing input",
        );
    }
}

fn focus_selected_pty(app: &mut App, pty_runtime: &mut PtyRuntime) {
    if app.selected_chat_id().is_some() {
        focus_selected_chat_agent(app, pty_runtime);
    } else {
        focus_selected_terminal(app, pty_runtime);
    }
}

fn resize_visible_terminal(app: &mut App, pty_runtime: &mut PtyRuntime, frame_area: Rect) {
    let Some((terminal_id, area)) = ui::selected_terminal_output_area(app, frame_area) else {
        return;
    };
    let size = pty_dimensions_from_area(area);
    app.resize_terminal_buffer(terminal_id, size.rows, size.cols);

    if pty_runtime.is_running(terminal_id) {
        let _ = pty_runtime.resize(terminal_id, size);
    }
}

fn resize_visible_chat_agent(app: &mut App, pty_runtime: &mut PtyRuntime, frame_area: Rect) {
    let Some((chat_id, area)) = ui::selected_chat_agent_output_area(app, frame_area) else {
        return;
    };
    let terminal_id = chat_agent_terminal_id(chat_id);
    let size = pty_dimensions_from_area(area);
    app.resize_terminal_buffer(terminal_id, size.rows, size.cols);

    if pty_runtime.is_running(terminal_id) {
        let _ = pty_runtime.resize(terminal_id, size);
    }
}

fn selected_terminal_dimensions(
    app: &App,
    frame_area: Rect,
    terminal_id: crate::model::TerminalId,
) -> Option<PtyDimensions> {
    ui::selected_terminal_output_area(app, frame_area)
        .filter(|(selected_terminal, _)| *selected_terminal == terminal_id)
        .map(|(_, area)| pty_dimensions_from_area(area))
}

fn selected_chat_agent_dimensions(
    app: &App,
    frame_area: Rect,
    chat_id: crate::model::ChatId,
) -> Option<PtyDimensions> {
    ui::selected_chat_agent_output_area(app, frame_area)
        .filter(|(selected_chat, _)| *selected_chat == chat_id)
        .map(|(_, area)| pty_dimensions_from_area(area))
}

fn pty_dimensions_from_area(area: Rect) -> PtyDimensions {
    PtyDimensions {
        rows: area.height.max(1),
        cols: area.width.max(1),
    }
}

fn drain_pty_events(app: &mut App, pty_runtime: &mut PtyRuntime) {
    for event in pty_runtime.drain_events() {
        match event {
            PtyEvent::Output { terminal, text } => app.append_terminal_output(terminal, &text),
            PtyEvent::Exited { terminal, status } => {
                if let Some(chat_id) = chat_id_from_agent_terminal_id(terminal) {
                    let chat_status = if status.code == 0 {
                        ChatStatus::Done
                    } else {
                        ChatStatus::Failed
                    };
                    app.mark_chat_status_by_id(chat_id, chat_status);
                    if app.pty_input_target() == Some(terminal) {
                        app.end_pty_input();
                    }
                    app.append_terminal_system_line(
                        terminal,
                        format!("pi agent exited: {}", status.label()),
                    );
                } else {
                    app.mark_terminal_stopped(terminal);
                    if app.terminal_input_target() == Some(terminal) {
                        app.end_terminal_input();
                    }
                    app.append_terminal_system_line(
                        terminal,
                        format!("PTY exited: {}", status.label()),
                    );
                }
            }
            PtyEvent::Error { terminal, message } => {
                app.append_terminal_system_line(terminal, message);
            }
        }
    }
}

fn key_to_pty_bytes(key: KeyEvent) -> Vec<u8> {
    let Some(mut bytes) = base_key_to_pty_bytes(key) else {
        return Vec::new();
    };

    if key.modifiers.contains(KeyModifiers::ALT) {
        let mut prefixed = Vec::with_capacity(bytes.len() + 1);
        prefixed.push(0x1b);
        prefixed.append(&mut bytes);
        prefixed
    } else {
        bytes
    }
}

fn base_key_to_pty_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    Some(match key.code {
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(5) => b"\x1b[15~".to_vec(),
        KeyCode::F(6) => b"\x1b[17~".to_vec(),
        KeyCode::F(7) => b"\x1b[18~".to_vec(),
        KeyCode::F(8) => b"\x1b[19~".to_vec(),
        KeyCode::F(9) => b"\x1b[20~".to_vec(),
        KeyCode::F(10) => b"\x1b[21~".to_vec(),
        KeyCode::F(11) => b"\x1b[23~".to_vec(),
        KeyCode::F(12) => b"\x1b[24~".to_vec(),
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            vec![control_byte(c)?]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        _ => return None,
    })
}

fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn drain_agent_events(app: &mut App, backend: &mut impl AgentBackend) {
    for event in backend.drain_events() {
        app.apply_agent_event(event);
    }
}

fn save_if_dirty(app: &mut App) -> io::Result<()> {
    if app.is_dirty() {
        storage::save(&app.project)?;
        app.mark_clean();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_agent_command_parses_from_env_style_string() {
        let command =
            parse_process_agent_command("agent-cli --model local").expect("command parses");

        assert_eq!(command.program, "agent-cli");
        assert_eq!(command.args, vec!["--model", "local"]);
        assert_eq!(command.label(), "agent-cli --model local");
    }

    #[test]
    fn blank_process_agent_command_is_ignored() {
        assert_eq!(parse_process_agent_command("   "), None);
    }

    #[test]
    fn pi_command_comes_from_config_with_default_fallback() {
        assert_eq!(
            pi_command(&Config {
                pi_agent_command: "pi -c".to_string(),
                auto_start_pi_agent: false,
                auto_start_terminals: false,
            }),
            "pi -c"
        );
        assert_eq!(
            pi_command(&Config {
                pi_agent_command: "   ".to_string(),
                auto_start_pi_agent: false,
                auto_start_terminals: false,
            }),
            "pi"
        );
    }

    #[test]
    fn terminal_key_bytes_encode_printable_text() {
        let key = KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE);

        assert_eq!(key_to_pty_bytes(key), "é".as_bytes());
    }

    #[test]
    fn terminal_key_bytes_encode_control_keys() {
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![0x03]
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            vec![0x7f]
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            b"\r".to_vec()
        );
    }

    #[test]
    fn terminal_key_bytes_encode_navigation_and_alt_keys() {
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            b"\x1b[A".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
            b"\x1bx".to_vec()
        );
    }
}
