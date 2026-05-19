mod app;
mod model;
mod pty;
mod storage;
mod ui;

use std::{io, time::Duration};

use app::{App, Mode};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use model::TerminalLaunch;
use pty::{PtyDimensions, PtyEvent, PtyRuntime, PtySpawn};
use ratatui::{layout::Rect, DefaultTerminal};

fn main() -> io::Result<()> {
    let project = storage::load_or_default()?;
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, App::new(project));
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, mut app: App) -> io::Result<()> {
    let mut pty_runtime = PtyRuntime::default();
    let size = terminal.size()?;
    let mut frame_area = Rect::new(0, 0, size.width, size.height);

    while !app.should_quit {
        drain_pty_events(&mut app, &mut pty_runtime);
        resize_visible_terminal(&app, &mut pty_runtime, frame_area);
        frame_area = terminal.draw(|frame| ui::draw(frame, &app))?.area;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(&mut app, &mut pty_runtime, key, frame_area);
                    save_if_dirty(&mut app)?;
                }
            }
        }
    }

    save_if_dirty(&mut app)?;
    Ok(())
}

fn handle_key(app: &mut App, pty_runtime: &mut PtyRuntime, key: KeyEvent, frame_area: Rect) {
    if matches!(&app.mode, Mode::OpenWorkspace(_)) {
        handle_open_workspace_key(app, key);
    } else if matches!(&app.mode, Mode::NewTerminalCommand(_)) {
        handle_terminal_command_key(app, key);
    } else if matches!(&app.mode, Mode::TerminalInput { .. }) {
        handle_terminal_input_key(app, pty_runtime, key);
    } else {
        handle_normal_key(app, pty_runtime, key, frame_area);
    }
}

fn handle_normal_key(app: &mut App, pty_runtime: &mut PtyRuntime, key: KeyEvent, frame_area: Rect) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit(),
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_previous(),
        KeyCode::Char('w') => app.add_workspace(),
        KeyCode::Char('o') => app.begin_open_workspace(),
        KeyCode::Char('c') => app.add_chat_to_selected_workspace(),
        KeyCode::Char('t') => app.add_terminal_to_selected_workspace(),
        KeyCode::Char('d') => {
            app.begin_new_terminal_command();
        }
        KeyCode::Char('r') => app.rotate_selected_status(),
        KeyCode::Char('s') => start_selected_terminal(app, pty_runtime, frame_area),
        KeyCode::Char('x') => stop_selected_terminal(app, pty_runtime),
        KeyCode::Char('i') => focus_selected_terminal(app, pty_runtime),
        _ => {}
    }
}

fn handle_terminal_input_key(app: &mut App, pty_runtime: &mut PtyRuntime, key: KeyEvent) {
    if key.code == KeyCode::Esc {
        app.end_terminal_input();
        return;
    }

    let Some(terminal_id) = app.terminal_input_target() else {
        app.end_terminal_input();
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
            app.end_terminal_input();
        }
        Err(error) => {
            app.append_terminal_system_line(terminal_id, format!("failed to send input: {error}"));
            app.end_terminal_input();
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
    let launch_label = terminal.launch.label();
    spawn.size = selected_terminal_dimensions(app, frame_area, terminal_id).unwrap_or_default();
    let size = spawn.size;

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.mark_terminal_running(terminal_id);
            app.append_terminal_system_line(
                terminal_id,
                format!("started PTY: {launch_label} ({}x{})", size.cols, size.rows),
            );
        }
        Err(error) => {
            app.append_terminal_system_line(terminal_id, format!("failed to start PTY: {error}"));
        }
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

fn resize_visible_terminal(app: &App, pty_runtime: &mut PtyRuntime, frame_area: Rect) {
    let Some((terminal_id, area)) = ui::selected_terminal_output_area(app, frame_area) else {
        return;
    };

    if pty_runtime.is_running(terminal_id) {
        let _ = pty_runtime.resize(terminal_id, pty_dimensions_from_area(area));
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
                app.mark_terminal_stopped(terminal);
                if app.terminal_input_target() == Some(terminal) {
                    app.end_terminal_input();
                }
                app.append_terminal_system_line(
                    terminal,
                    format!("PTY exited: {}", status.label()),
                );
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
