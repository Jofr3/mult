mod app;
mod model;
mod pty;
mod storage;
mod ui;

use std::{io, time::Duration};

use app::App;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pty::{PtyDimensions, PtyEvent, PtyRuntime, PtySpawn};
use ratatui::DefaultTerminal;

fn main() -> io::Result<()> {
    let project = storage::load_or_default()?;
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, App::new(project));
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, mut app: App) -> io::Result<()> {
    let mut pty_runtime = PtyRuntime::default();

    while !app.should_quit {
        drain_pty_events(&mut app, &mut pty_runtime);
        terminal.draw(|frame| ui::draw(frame, &app))?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(&mut app, &mut pty_runtime, key);
                    save_if_dirty(&mut app)?;
                }
            }
        }
    }

    save_if_dirty(&mut app)?;
    Ok(())
}

fn handle_key(app: &mut App, pty_runtime: &mut PtyRuntime, key: KeyEvent) {
    if app.is_open_workspace_prompt_active() {
        handle_open_workspace_key(app, key);
    } else {
        handle_normal_key(app, pty_runtime, key);
    }
}

fn handle_normal_key(app: &mut App, pty_runtime: &mut PtyRuntime, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit(),
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_previous(),
        KeyCode::Char('w') => app.add_workspace(),
        KeyCode::Char('o') => app.begin_open_workspace(),
        KeyCode::Char('c') => app.add_chat_to_selected_workspace(),
        KeyCode::Char('t') => app.add_terminal_to_selected_workspace(),
        KeyCode::Char('r') => app.rotate_selected_status(),
        KeyCode::Char('s') => start_selected_terminal(app, pty_runtime),
        KeyCode::Char('x') => stop_selected_terminal(app, pty_runtime),
        _ => {}
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

fn start_selected_terminal(app: &mut App, pty_runtime: &mut PtyRuntime) {
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

    let spawn = PtySpawn::shell(
        terminal_id,
        workspace.cwd.clone(),
        workspace.environment.clone(),
    );
    let program = spawn.program.clone();

    match pty_runtime.start(spawn) {
        Ok(()) => {
            let _ = pty_runtime.resize(terminal_id, PtyDimensions::default());
            app.mark_terminal_running(terminal_id);
            app.append_terminal_system_line(terminal_id, format!("started PTY shell: {program}"));
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

fn drain_pty_events(app: &mut App, pty_runtime: &mut PtyRuntime) {
    for event in pty_runtime.drain_events() {
        match event {
            PtyEvent::Output { terminal, text } => app.append_terminal_output(terminal, &text),
            PtyEvent::Exited { terminal, status } => {
                app.mark_terminal_stopped(terminal);
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

fn save_if_dirty(app: &mut App) -> io::Result<()> {
    if app.is_dirty() {
        storage::save(&app.project)?;
        app.mark_clean();
    }

    Ok(())
}
