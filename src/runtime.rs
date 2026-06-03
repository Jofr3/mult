//! Runtime orchestration for the `mult` client: the event loop and all the
//! glue that drives `App`, the `PtyRuntime`, and the agent backend. `main.rs`
//! keeps only terminal setup/teardown and calls `runtime::run`.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use mult::{
    agent::{
        self, AgentBackend, AgentEvent, NoopAgentBackend, ProcessAgentBackend, ProcessAgentCommand,
    },
    app::{App, CommandAction, NavItem, Prompt, SelectionCell, TextSelection},
    config::Config,
    git,
    model::{self, AgentKind, ChatStatus, PtyKey, TerminalLaunch, TerminalStatus},
    pty::{PtyDimensions, PtyEvent, PtyRuntime, PtySpawn},
    storage, ui,
};
use ratatui::{layout::Rect, DefaultTerminal};
use serde::Deserialize;

const AGENT_CMD_ENV: &str = "MULT_AGENT_CMD";
const MULT_AGENT_STATUS_PATH_ENV: &str = "MULT_AGENT_STATUS_PATH";
const MULT_AGENT_CHAT_ID_ENV: &str = "MULT_AGENT_CHAT_ID";
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const READY_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(0);
const GIT_BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const MOUSE_SCROLL_ROWS: usize = 3;

#[derive(Debug, Deserialize)]
struct MultAgentStatusRecord {
    status: String,
}

const MULT_STATUS_EXTENSION_SOURCE: &str = include_str!("../extensions/mult-status.ts");
const MULT_CLAUDE_STATUS_SCRIPT_SOURCE: &str = include_str!("../extensions/mult-claude-status.sh");

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
    let mut parts = split_process_agent_command(raw).ok()?.into_iter();
    let program = parts.next()?;
    if program.is_empty() {
        return None;
    }

    Some(ProcessAgentCommand::with_args(program, parts))
}

fn split_process_agent_command(raw: &str) -> Result<Vec<String>, &'static str> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut escaping = false;
    let mut in_token = false;

    for ch in raw.chars() {
        if escaping {
            current.push(ch);
            escaping = false;
            in_token = true;
            continue;
        }

        match quote {
            Quote::None => match ch {
                '\\' => {
                    escaping = true;
                    in_token = true;
                }
                '\'' => {
                    quote = Quote::Single;
                    in_token = true;
                }
                '"' => {
                    quote = Quote::Double;
                    in_token = true;
                }
                ch if ch.is_whitespace() => {
                    if in_token {
                        args.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                }
                _ => {
                    current.push(ch);
                    in_token = true;
                }
            },
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => {
                    escaping = true;
                    in_token = true;
                }
                _ => current.push(ch),
            },
        }
    }

    if escaping {
        current.push('\\');
    }
    if quote != Quote::None {
        return Err("unterminated quote");
    }
    if in_token {
        args.push(current);
    }

    Ok(args)
}

pub fn run(terminal: &mut DefaultTerminal, mut app: App, config: Config) -> io::Result<()> {
    let mut pty_runtime = PtyRuntime::default();
    let mut agent_backend = RuntimeAgentBackend::from_env();
    let size = terminal.size()?;
    let mut frame_area = Rect::new(0, 0, size.width, size.height);
    restore_persisted_sessions(&mut app, &mut pty_runtime, &config, frame_area);
    refresh_workspace_git_branches(&mut app);
    let mut last_git_branch_refresh = Instant::now();

    // The screen is static unless something changes, so only rebuild a frame
    // when needed instead of every ~16ms tick. The tick still runs so PTY/agent
    // output (delivered over channels, not via event::poll) is drained promptly;
    // it is just the expensive draw that is gated. `needs_redraw` is set by any
    // input event, drained PTY/agent/status change, git-branch refresh, or an
    // auto-start/resize that altered state.
    let mut needs_redraw = true;
    while !app.should_quit {
        if last_git_branch_refresh.elapsed() >= GIT_BRANCH_REFRESH_INTERVAL {
            refresh_workspace_git_branches(&mut app);
            last_git_branch_refresh = Instant::now();
            needs_redraw = true;
        }
        needs_redraw |= drain_pty_events(&mut app, &mut pty_runtime);
        needs_redraw |= drain_agent_events(&mut app, &mut agent_backend);
        needs_redraw |= drain_mult_agent_status_events(&mut app);
        save_if_dirty(&mut app)?;
        needs_redraw |= resize_visible_terminal(&mut app, &mut pty_runtime, &config, frame_area);
        needs_redraw |= resize_visible_chat_agent(&mut app, &mut pty_runtime, &config, frame_area);
        needs_redraw |=
            auto_start_selected_terminal(&mut app, &mut pty_runtime, &config, frame_area);
        needs_redraw |=
            auto_start_selected_chat_agent(&mut app, &mut pty_runtime, &config, frame_area);

        if needs_redraw {
            frame_area = terminal
                .draw(|frame| ui::draw(frame, &app, &pty_runtime, &config))?
                .area;
            needs_redraw = false;
        }

        if event::poll(EVENT_POLL_INTERVAL)? {
            handle_event(
                &mut app,
                &mut pty_runtime,
                &config,
                event::read()?,
                frame_area,
            );
            needs_redraw = true;
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

fn refresh_workspace_git_branches(app: &mut App) {
    let branches = app
        .project
        .workspaces
        .iter()
        .map(|workspace| {
            let branch = workspace.cwd.as_deref().and_then(git::current_branch);
            (workspace.id, branch)
        })
        .collect::<Vec<_>>();
    app.replace_workspace_git_branches(branches);
}

fn restore_persisted_sessions(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    let terminals = app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.terminals.iter().filter_map(|terminal| {
                (terminal.status == TerminalStatus::Running).then_some((workspace.id, terminal.id))
            })
        })
        .collect::<Vec<_>>();

    for (workspace, terminal) in terminals {
        start_terminal(app, pty_runtime, config, frame_area, workspace, terminal);
    }

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
        start_or_focus_chat_agent(app, pty_runtime, config, frame_area, workspace, chat, false);
    }
}

fn handle_event(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    event: Event,
    frame_area: Rect,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, pty_runtime, config, key, frame_area);
        }
        Event::Mouse(mouse) => handle_mouse(app, pty_runtime, mouse, frame_area),
        Event::Paste(text) => handle_paste(app, pty_runtime, config, text, frame_area),
        _ => {}
    }
}

fn handle_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    frame_area: Rect,
) {
    if is_quit_key(key) {
        app.quit();
        return;
    }

    match &app.prompt {
        Some(Prompt::OpenWorkspace(_)) => handle_open_workspace_key(app, config, key),
        Some(Prompt::NewTerminalCommand(_)) => handle_terminal_command_key(app, key),
        Some(Prompt::CommandPalette(_)) => {
            handle_command_palette_key(app, pty_runtime, config, key, frame_area);
        }
        Some(Prompt::Search(_)) => handle_search_key(app, key),
        None => handle_unprompted_key(app, pty_runtime, config, key, frame_area),
    }
}

fn handle_mouse(app: &mut App, pty_runtime: &mut PtyRuntime, mouse: MouseEvent, frame_area: Rect) {
    if app.is_prompt_active() {
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            begin_text_selection_at_mouse(app, frame_area, mouse);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            update_text_selection_at_mouse(app, frame_area, mouse);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            finish_text_selection_at_mouse(app, pty_runtime, frame_area, mouse);
        }
        MouseEventKind::ScrollUp => {
            scroll_output_at_mouse(app, pty_runtime, frame_area, mouse, ScrollDirection::Up);
        }
        MouseEventKind::ScrollDown => {
            scroll_output_at_mouse(app, pty_runtime, frame_area, mouse, ScrollDirection::Down);
        }
        _ => {}
    }
}

fn handle_paste(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    text: String,
    frame_area: Rect,
) {
    if app.is_prompt_active() {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            app.push_prompt_char(ch);
        }
        return;
    }

    let Some(terminal_id) = start_selected_pty_if_needed(app, pty_runtime, config, frame_area)
    else {
        return;
    };

    match pty_runtime.send_paste(terminal_id, &text) {
        Ok(true) => {}
        Ok(false) => {
            pty_runtime.append_terminal_system_line(terminal_id, "PTY is not running");
        }
        Err(error) => {
            pty_runtime
                .append_terminal_system_line(terminal_id, format!("failed to paste: {error}"));
        }
    }
}

fn begin_text_selection_at_mouse(app: &mut App, frame_area: Rect, mouse: MouseEvent) -> bool {
    let Some((terminal, area)) = selected_output_area(app, frame_area) else {
        app.clear_text_selection();
        return false;
    };
    if !rect_contains(area, mouse.column, mouse.row) {
        app.clear_text_selection();
        return false;
    }
    let Some(cell) = mouse_cell_in_area(area, mouse.column, mouse.row) else {
        return false;
    };
    app.begin_text_selection(terminal, cell);
    true
}

fn update_text_selection_at_mouse(app: &mut App, frame_area: Rect, mouse: MouseEvent) -> bool {
    let Some((terminal, cell)) = active_selection_cell_at_mouse(app, frame_area, mouse) else {
        return false;
    };
    app.update_text_selection(terminal, cell)
}

fn finish_text_selection_at_mouse(
    app: &mut App,
    pty_runtime: &PtyRuntime,
    frame_area: Rect,
    mouse: MouseEvent,
) -> bool {
    let Some((terminal, cell)) = active_selection_cell_at_mouse(app, frame_area, mouse) else {
        return false;
    };
    let Some(selection) = app.end_text_selection(terminal, cell) else {
        return false;
    };
    if selection.anchor == selection.focus {
        app.clear_text_selection();
        return false;
    }
    let _ = copy_text_selection_to_clipboard(pty_runtime, selection);
    true
}

fn copy_current_text_selection(app: &App, pty_runtime: &PtyRuntime) -> bool {
    let Some(selection) = app.text_selection else {
        return false;
    };
    copy_text_selection_to_clipboard(pty_runtime, selection)
}

fn copy_text_selection_to_clipboard(pty_runtime: &PtyRuntime, selection: TextSelection) -> bool {
    if selection.anchor == selection.focus {
        return false;
    }
    let Some(text) = selected_text(pty_runtime, selection) else {
        return false;
    };
    copy_text_to_clipboard(&text).is_ok()
}

fn active_selection_cell_at_mouse(
    app: &App,
    frame_area: Rect,
    mouse: MouseEvent,
) -> Option<(PtyKey, SelectionCell)> {
    let selection = app.text_selection?;
    let (terminal, area) = selected_output_area(app, frame_area)?;
    if terminal != selection.terminal {
        return None;
    }
    mouse_cell_in_area(area, mouse.column, mouse.row).map(|cell| (terminal, cell))
}

fn selected_output_area(app: &App, frame_area: Rect) -> Option<(PtyKey, Rect)> {
    if let Some((terminal, area)) = ui::selected_terminal_output_area(app, frame_area) {
        return Some((PtyKey::Terminal(terminal), area));
    }
    ui::selected_chat_agent_output_area(app, frame_area)
        .map(|(chat, area)| (PtyKey::ChatAgent(chat), area))
}

fn mouse_cell_in_area(area: Rect, column: u16, row: u16) -> Option<SelectionCell> {
    if area.is_empty() {
        return None;
    }
    Some(SelectionCell {
        row: i32::from(
            row.saturating_sub(area.y)
                .min(area.height.saturating_sub(1)),
        ),
        col: column
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(1)),
    })
}

fn selected_text(pty_runtime: &PtyRuntime, selection: TextSelection) -> Option<String> {
    let parser = pty_runtime.parser(selection.terminal)?;
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return None;
    }

    let range = selection.normalized_range();
    let visible_last_row = i32::from(rows.saturating_sub(1));
    if range.end.row < 0 || range.start.row > visible_last_row {
        return None;
    }

    let start_row = range.start.row.max(0);
    let end_row = range.end.row.min(visible_last_row);
    let start_col = if start_row == range.start.row {
        range.start.col.min(cols.saturating_sub(1))
    } else {
        0
    };
    let end_col = if end_row == range.end.row {
        range.end.col.min(cols.saturating_sub(1))
    } else {
        cols.saturating_sub(1)
    };
    let start_row = u16::try_from(start_row).unwrap_or(0);
    let end_row = u16::try_from(end_row).unwrap_or(rows.saturating_sub(1));
    let end_col_exclusive = end_col.saturating_add(1).min(cols);
    if start_row == end_row && start_col >= end_col_exclusive {
        return None;
    }

    let text = screen.contents_between(start_row, start_col, end_row, end_col_exclusive);
    (!text.is_empty()).then_some(text)
}

fn copy_text_to_clipboard(text: &str) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let bits = ((first as u32) << 16) | ((second as u32) << 8) | third as u32;

        output.push(TABLE[((bits >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((bits >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[((bits >> 6) & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(bits & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollDirection {
    Up,
    Down,
}

fn scroll_output_at_mouse(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    frame_area: Rect,
    mouse: MouseEvent,
    direction: ScrollDirection,
) -> bool {
    let Some((terminal, area)) = output_terminal_at(app, frame_area, mouse.column, mouse.row)
    else {
        return false;
    };

    // A program that has grabbed the mouse (Claude Code, nvim, less, ...)
    // scrolls its own view. Our local scrollback holds nothing for it — the
    // alternate screen keeps none — so hand the wheel notch to the program
    // instead of swallowing it into a buffer that can never move.
    if pty_runtime.terminal_reports_mouse(terminal) {
        let Some(cell) = mouse_cell_in_area(area, mouse.column, mouse.row) else {
            return false;
        };
        let col = cell.col.saturating_add(1);
        let row = u16::try_from(cell.row).unwrap_or(0).saturating_add(1);
        return pty_runtime.forward_wheel(terminal, direction == ScrollDirection::Up, col, row);
    }

    match direction {
        ScrollDirection::Up => {
            scroll_terminal_output_up(app, pty_runtime, terminal, MOUSE_SCROLL_ROWS)
        }
        ScrollDirection::Down => {
            scroll_terminal_output_down(app, pty_runtime, terminal, MOUSE_SCROLL_ROWS)
        }
    }
}

fn output_terminal_at(
    app: &App,
    frame_area: Rect,
    column: u16,
    row: u16,
) -> Option<(PtyKey, Rect)> {
    selected_output_area(app, frame_area).filter(|(_, area)| rect_contains(*area, column, row))
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn handle_unprompted_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    frame_area: Rect,
) {
    if handle_control_key(app, pty_runtime, config, key, frame_area) {
        return;
    }

    handle_selected_pty_input_key(app, pty_runtime, config, key, frame_area);
}

fn handle_control_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    frame_area: Rect,
) -> bool {
    if is_shifted_control_char(key, 'c') {
        let _ = copy_current_text_selection(app, pty_runtime);
        return true;
    }

    if is_control_down_key(key) {
        app.select_next();
        return true;
    }
    if is_control_up_key(key) {
        app.select_previous();
        return true;
    }
    if is_unshifted_control_char(key, 'q') {
        delete_selected_now(app, pty_runtime);
        return true;
    }
    if is_unshifted_control_char(key, 'p') {
        app.begin_command_palette();
        return true;
    }
    if is_unshifted_control_char(key, 's') && app.begin_search() {
        return true;
    }
    if is_unshifted_control_char(key, 'a') {
        add_agent_to_selected_workspace(app, pty_runtime, config, frame_area, AgentKind::Pi);
        return true;
    }
    if is_unshifted_control_char(key, 'x') {
        add_agent_to_selected_workspace(
            app,
            pty_runtime,
            config,
            frame_area,
            AgentKind::ClaudeCode,
        );
        return true;
    }
    if is_unshifted_control_char(key, 't') {
        app.add_terminal_to_selected_workspace();
        return true;
    }
    if is_unshifted_control_char(key, 'f') {
        app.begin_open_workspace(&config.projects);
        return true;
    }

    false
}

fn is_quit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc) && is_control_key(key)
}

fn is_control_down_key(key: KeyEvent) -> bool {
    is_unshifted_control_char(key, 'j')
        || (matches!(key.code, KeyCode::Enter) && is_control_key(key))
}

fn is_control_up_key(key: KeyEvent) -> bool {
    is_unshifted_control_char(key, 'k')
}

fn is_unshifted_control_char(key: KeyEvent, target: char) -> bool {
    let KeyCode::Char(ch) = key.code else {
        return false;
    };

    is_control_key(key)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && ch == target.to_ascii_lowercase()
}

fn is_shifted_control_char(key: KeyEvent, target: char) -> bool {
    let KeyCode::Char(ch) = key.code else {
        return false;
    };

    is_control_key(key)
        && key.modifiers.contains(KeyModifiers::SHIFT)
        && ch.eq_ignore_ascii_case(&target)
}

fn is_control_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

fn scroll_terminal_output_up(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    terminal: PtyKey,
    rows: usize,
) -> bool {
    let before = terminal_scrollback(pty_runtime, terminal);
    let changed = pty_runtime.scroll_up(terminal, rows).unwrap_or(false);
    sync_text_selection_with_scrollback(app, pty_runtime, terminal, before, changed);
    changed
}

fn scroll_terminal_output_down(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    terminal: PtyKey,
    rows: usize,
) -> bool {
    let before = terminal_scrollback(pty_runtime, terminal);
    let changed = pty_runtime.scroll_down(terminal, rows).unwrap_or(false);
    sync_text_selection_with_scrollback(app, pty_runtime, terminal, before, changed);
    changed
}

fn terminal_scrollback(pty_runtime: &PtyRuntime, terminal: PtyKey) -> usize {
    pty_runtime
        .parser(terminal)
        .map(|parser| parser.screen().scrollback())
        .unwrap_or_default()
}

fn sync_text_selection_with_scrollback(
    app: &mut App,
    pty_runtime: &PtyRuntime,
    terminal: PtyKey,
    before: usize,
    changed: bool,
) {
    if !changed {
        return;
    }
    let after = terminal_scrollback(pty_runtime, terminal);
    let delta = (after as i64 - before as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    app.shift_text_selection_rows(terminal, delta);
}

fn add_agent_to_selected_workspace(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
    agent: AgentKind,
) {
    if let Some((workspace, chat)) = app.add_chat_to_selected_workspace_and_return(agent) {
        start_or_focus_chat_agent(app, pty_runtime, config, frame_area, workspace, chat, true);
    }
}

fn delete_selected_now(app: &mut App, pty_runtime: &mut PtyRuntime) {
    for terminal in app.delete_selected_immediately() {
        let _ = pty_runtime.stop(terminal);
        pty_runtime.remove_terminal(terminal);
    }
}

fn handle_selected_pty_input_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    frame_area: Rect,
) {
    // Emptiness does not depend on cursor-key mode, so this also avoids starting
    // a PTY for keys that map to nothing (e.g. shortcuts handled elsewhere).
    if key_to_pty_bytes_in_mode(key, false).is_empty() {
        return;
    }

    let Some(terminal_id) = start_selected_pty_if_needed(app, pty_runtime, config, frame_area)
    else {
        return;
    };

    // Honour the application cursor-key mode (DECCKM) the PTY program requested,
    // so arrows reach full-screen apps in the SS3 form they expect.
    let application_cursor = pty_runtime
        .parser(terminal_id)
        .is_some_and(|parser| parser.screen().application_cursor());
    let bytes = key_to_pty_bytes_in_mode(key, application_cursor);

    match pty_runtime.send_input(terminal_id, &bytes) {
        Ok(true) => {}
        Ok(false) => {
            pty_runtime.append_terminal_system_line(terminal_id, "PTY is not running");
        }
        Err(error) => {
            pty_runtime
                .append_terminal_system_line(terminal_id, format!("failed to send input: {error}"));
        }
    }
}

fn start_selected_pty_if_needed(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) -> Option<PtyKey> {
    match app.selected_item()? {
        NavItem::Chat { workspace, chat } => {
            let terminal = PtyKey::ChatAgent(chat);
            if pty_runtime.is_running(terminal) {
                app.begin_chat_agent_input();
            } else {
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
            pty_runtime.is_running(terminal).then_some(terminal)
        }
        NavItem::Terminal {
            workspace,
            terminal,
        } => {
            let key = PtyKey::Terminal(terminal);
            if !pty_runtime.is_running(key) {
                start_terminal(app, pty_runtime, config, frame_area, workspace, terminal);
            }
            if pty_runtime.is_running(key) {
                app.begin_terminal_input();
                Some(key)
            } else {
                None
            }
        }
    }
}

fn handle_open_workspace_key(app: &mut App, config: &Config, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.cancel_prompt(),
        KeyCode::Enter => app.submit_open_workspace(&config.projects),
        KeyCode::Up => app.select_previous_open_workspace_match(&config.projects),
        KeyCode::Down => app.select_next_open_workspace_match(&config.projects),
        _ if is_unshifted_control_char(key, 'k') => {
            app.select_previous_open_workspace_match(&config.projects);
        }
        _ if is_unshifted_control_char(key, 'j') => {
            app.select_next_open_workspace_match(&config.projects);
        }
        KeyCode::Backspace => app.pop_prompt_char(),
        _ if is_unshifted_control_char(key, 'c') => app.cancel_prompt(),
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
        _ if is_unshifted_control_char(key, 'c') => app.cancel_prompt(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.push_prompt_char(c);
        }
        _ => {}
    }
}

fn handle_command_palette_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    frame_area: Rect,
) {
    match key.code {
        KeyCode::Esc => app.cancel_prompt(),
        KeyCode::Enter => {
            if let Some(action) = app.submit_command_palette() {
                execute_command_action(app, pty_runtime, config, action, frame_area);
            }
        }
        KeyCode::Up => app.select_previous_command_palette_entry(),
        KeyCode::Down => app.select_next_command_palette_entry(),
        _ if is_unshifted_control_char(key, 'k') => app.select_previous_command_palette_entry(),
        _ if is_unshifted_control_char(key, 'j') => app.select_next_command_palette_entry(),
        KeyCode::Backspace => app.pop_prompt_char(),
        _ if is_unshifted_control_char(key, 'c') => app.cancel_prompt(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.push_prompt_char(c);
        }
        _ => {}
    }
}

fn handle_search_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.cancel_prompt(),
        KeyCode::Enter => app.submit_search(),
        KeyCode::Backspace => app.pop_prompt_char(),
        _ if is_unshifted_control_char(key, 'c') => app.cancel_prompt(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.push_prompt_char(c);
        }
        _ => {}
    }
}

fn execute_command_action(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    action: CommandAction,
    frame_area: Rect,
) {
    match action {
        CommandAction::FocusSidebar => app.focus_sidebar(),
        CommandAction::FocusSelectedPane => {
            app.focus_selected_main();
        }
        CommandAction::StartInput => focus_selected_input(app, pty_runtime, config, frame_area),
        CommandAction::AddAgentChat => {
            add_agent_to_selected_workspace(app, pty_runtime, config, frame_area, AgentKind::Pi);
        }
        CommandAction::AddClaudeCodeChat => {
            add_agent_to_selected_workspace(
                app,
                pty_runtime,
                config,
                frame_area,
                AgentKind::ClaudeCode,
            );
        }
        CommandAction::AddShellTerminal => app.add_terminal_to_selected_workspace(),
        CommandAction::AddCommandTerminal => {
            app.begin_new_terminal_command();
        }
        CommandAction::OpenWorkspace => app.begin_open_workspace(&config.projects),
        CommandAction::DeleteSelected => delete_selected_now(app, pty_runtime),
        CommandAction::SearchSelectedPane => {
            app.begin_search();
        }
        CommandAction::ClearSearch => app.clear_search(),
        CommandAction::Quit => app.quit(),
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

fn start_terminal(
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
            app.mark_terminal_running(terminal_id);
            true
        }
        Err(error) => {
            pty_runtime.append_terminal_system_line(
                key,
                format!("failed to start terminal `{terminal_name}`: {error}"),
            );
            app.mark_terminal_stopped(terminal_id);
            false
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

    let Some(workspace) = app.project.workspace(workspace_id) else {
        return;
    };
    let (chat_name, agent) = workspace
        .chats
        .iter()
        .find(|chat| chat.id == chat_id)
        .map(|chat| (chat.name.clone(), chat.agent))
        .unwrap_or_else(|| (format!("chat {}", chat_id.0), AgentKind::default()));
    let command = agent_command(config, agent);
    let status_path = mult_agent_status_path(chat_id);
    let _ = fs::remove_file(&status_path);
    let mut environment = workspace.environment.clone();
    environment.insert(
        MULT_AGENT_STATUS_PATH_ENV.to_string(),
        status_path.display().to_string(),
    );
    environment.insert(MULT_AGENT_CHAT_ID_ENV.to_string(), chat_id.0.to_string());
    let mut spawn = PtySpawn::command_line(
        terminal_id,
        command.clone(),
        workspace.cwd.clone(),
        environment,
    );
    spawn.size = chat_agent_dimensions(app, frame_area);

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Idle);
            if focus_after_start {
                app.begin_chat_agent_input();
            }
        }
        Err(error) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
            pty_runtime.append_terminal_system_line(
                terminal_id,
                format!(
                    "failed to start {} agent for `{chat_name}`: {error}",
                    agent.display_name()
                ),
            );
        }
    }
}

fn auto_start_selected_terminal(
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
    let key = PtyKey::Terminal(terminal_id);
    if pty_runtime.is_running(key) || !pty_runtime.terminal_output_is_blank(key) {
        return false;
    }

    start_selected_terminal(app, pty_runtime, config, frame_area);
    true
}

fn auto_start_selected_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
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
    if pty_runtime.is_running(terminal_id) || !pty_runtime.terminal_output_is_blank(terminal_id) {
        return false;
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

/// Build the shell command line that backs a chat, chosen by its agent kind.
/// Both backends report status into the same per-chat file that `mult` polls,
/// but through different mechanisms: pi loads a bundled extension (`-e`), while
/// Claude Code gets a generated hooks settings file (`--settings`).
fn agent_command(config: &Config, agent: AgentKind) -> String {
    match agent {
        AgentKind::Pi => pi_command_with_mult_status_extension(config),
        AgentKind::ClaudeCode => claude_code_command_with_mult_status_hooks(config),
    }
}

fn pi_command(config: &Config) -> String {
    let command = config.pi_agent_command.trim();
    if command.is_empty() {
        "pi".to_string()
    } else {
        command.to_string()
    }
}

fn claude_code_command(config: &Config) -> String {
    let command = config.claude_code_command.trim();
    if command.is_empty() {
        "claude".to_string()
    } else {
        command.to_string()
    }
}

fn pi_command_with_mult_status_extension(config: &Config) -> String {
    let command = pi_command(config);
    let Some(extension) = write_mult_status_extension_file() else {
        return command;
    };

    format!(
        "{command} -e {}",
        shell_quote(&extension.display().to_string())
    )
}

/// Append `--settings <file>` pointing at a generated hooks file that reports
/// chat status into the file `mult` polls. `--settings` merges over the user's
/// own Claude Code settings for this session only, so it does not touch their
/// config on disk. If the files cannot be written, fall back to the plain
/// command — Claude Code still runs, just without a live status dot.
fn claude_code_command_with_mult_status_hooks(config: &Config) -> String {
    let command = claude_code_command(config);
    let Some(settings) = write_mult_claude_status_files() else {
        return command;
    };

    format!(
        "{command} --settings {}",
        shell_quote(&settings.display().to_string())
    )
}

fn write_mult_status_extension_file() -> Option<PathBuf> {
    let dir = ensure_mult_runtime_dir().ok()?;
    write_private_runtime_file(
        &dir,
        "mult-status-extension",
        "ts",
        MULT_STATUS_EXTENSION_SOURCE.as_bytes(),
    )
}

/// Write the bundled status-writer script and a Claude Code settings file whose
/// hooks invoke it, returning the settings path to hand to `--settings`. Two
/// files because the settings JSON must reference the script by absolute path.
fn write_mult_claude_status_files() -> Option<PathBuf> {
    let dir = ensure_mult_runtime_dir().ok()?;
    let script = write_private_runtime_file(
        &dir,
        "mult-claude-status",
        "sh",
        MULT_CLAUDE_STATUS_SCRIPT_SOURCE.as_bytes(),
    )?;
    let settings = mult_claude_status_settings_json(&script);
    write_private_runtime_file(&dir, "mult-claude-settings", "json", settings.as_bytes())
}

/// Build the Claude Code `--settings` JSON that maps lifecycle hook events to
/// `mult` statuses by invoking the bundled script with the status as its
/// argument. Built with `serde_json` so the script path is correctly escaped
/// into the embedded shell command.
fn mult_claude_status_settings_json(script: &Path) -> String {
    let script = shell_quote(&script.display().to_string());
    let hook = |status: &str| {
        serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": format!("sh {script} {status}") }],
        })
    };

    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [hook("idle")],
            "UserPromptSubmit": [hook("running")],
            "PreToolUse": [hook("running")],
            "Notification": [hook("waiting")],
            "Stop": [hook("finished")],
        },
    });

    serde_json::to_string(&settings).unwrap_or_default()
}

fn write_private_runtime_file(
    dir: &Path,
    prefix: &str,
    extension: &str,
    contents: &[u8],
) -> Option<PathBuf> {
    for _ in 0..16 {
        let path = dir.join(format!(
            "{prefix}-{}-{:016x}.{extension}",
            std::process::id(),
            random_u64().ok()?
        ));
        match write_private_file(&path, contents) {
            Ok(()) => return Some(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '+'))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

fn focus_selected_input(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    if app.selected_chat_id().is_some() {
        start_or_focus_selected_chat_agent(app, pty_runtime, config, frame_area);
    } else if app.selected_terminal_id().is_some() {
        start_or_focus_selected_terminal(app, pty_runtime, config, frame_area);
    }
}

fn start_or_focus_selected_terminal(
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

fn resize_visible_terminal(
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
    let changed = pty_dimensions_changed(pty_runtime, key, size);
    let _ = pty_runtime.resize(key, size);
    changed
}

fn resize_visible_chat_agent(
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
    let changed = pty_dimensions_changed(pty_runtime, terminal_id, size);
    let _ = pty_runtime.resize(terminal_id, size);
    changed
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

fn chat_agent_dimensions(app: &App, frame_area: Rect) -> PtyDimensions {
    pty_dimensions_from_area(ui::chat_agent_output_area_for(app, frame_area))
}

fn pty_dimensions_from_area(area: Rect) -> PtyDimensions {
    PtyDimensions {
        rows: area.height.max(1),
        cols: area.width.max(1),
    }
}

fn drain_pty_events(app: &mut App, pty_runtime: &mut PtyRuntime) -> bool {
    let mut changed = false;
    for event in pty_runtime.drain_events() {
        changed = true;
        match event {
            PtyEvent::Scrollback { .. } | PtyEvent::Output { .. } => {}
            PtyEvent::Exited { terminal, status } => match terminal {
                PtyKey::ChatAgent(chat_id) => {
                    let chat_status = if status.code == 0 {
                        ChatStatus::Done
                    } else {
                        ChatStatus::Failed
                    };
                    let agent = chat_agent_kind(app, chat_id);
                    app.mark_chat_status_by_id(chat_id, chat_status);
                    if app.pty_input_target() == Some(terminal) {
                        app.end_pty_input();
                    }
                    let exit_message =
                        format!("{} agent exited: {}", agent.display_name(), status.label());
                    pty_runtime.append_terminal_system_line(terminal, exit_message.as_str());
                }
                PtyKey::Terminal(terminal_id) => {
                    app.mark_terminal_stopped(terminal_id);
                    if app.terminal_input_target() == Some(terminal_id) {
                        app.end_terminal_input();
                    }
                    let exit_message = format!("PTY exited: {}", status.label());
                    pty_runtime.append_terminal_system_line(terminal, exit_message.as_str());
                }
            },
            PtyEvent::Error { terminal, message } => {
                pty_runtime.append_terminal_system_line(terminal, message.as_str());
            }
        }
    }
    changed
}

#[cfg(test)]
fn key_to_pty_bytes(key: KeyEvent) -> Vec<u8> {
    key_to_pty_bytes_in_mode(key, false)
}

fn key_to_pty_bytes_in_mode(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    // Keys that emit their own escape sequence must use xterm's CSI modifier
    // encoding (`CSI 1 ; <mod> <final>` or `CSI <n> ; <mod> ~`) when a modifier
    // is held. Prefixing such a sequence with ESC — the meta convention for
    // plain characters — would send e.g. Alt+Left as `\x1b\x1b[D`, which the PTY
    // application renders as literal characters instead of moving the cursor.
    // Modified cursor keys always use the CSI form, regardless of cursor-key mode.
    if let Some(modifier) = xterm_modifier_code(key.modifiers) {
        if let Some(final_byte) = csi_letter_key(key.code) {
            return format!("\x1b[1;{modifier}{final_byte}").into_bytes();
        }
        if let Some(number) = csi_tilde_key(key.code) {
            return format!("\x1b[{number};{modifier}~").into_bytes();
        }
    }

    let Some(mut bytes) = base_key_to_pty_bytes(key, application_cursor) else {
        return Vec::new();
    };

    // Meta convention: Alt+<key> is the base byte(s) prefixed with ESC, e.g.
    // Alt+b -> `\x1bb`, Alt+Backspace -> `\x1b\x7f` (delete previous word).
    if key.modifiers.contains(KeyModifiers::ALT) {
        let mut prefixed = Vec::with_capacity(bytes.len() + 1);
        prefixed.push(0x1b);
        prefixed.append(&mut bytes);
        prefixed
    } else {
        bytes
    }
}

/// xterm modifier parameter for CSI-encoded keys: `1` plus a bitmask of
/// Shift (1), Alt (2), and Ctrl (4). Returns `None` when none of those are held
/// so that unmodified keys keep their plain escape sequence.
fn xterm_modifier_code(modifiers: KeyModifiers) -> Option<u8> {
    let mut bits = 0u8;
    if modifiers.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    (bits != 0).then_some(bits + 1)
}

/// Unmodified cursor-key sequence: SS3 (`ESC O <final>`) when the application
/// has enabled DECCKM (e.g. vim, less, fzf), CSI (`ESC [ <final>`) otherwise.
fn cursor_key_bytes(application_cursor: bool, final_byte: char) -> Vec<u8> {
    let introducer = if application_cursor { "\x1bO" } else { "\x1b[" };
    format!("{introducer}{final_byte}").into_bytes()
}

/// Final byte for keys encoded as `CSI 1 ; <mod> <final>` when modified:
/// arrows, Home/End, and F1–F4.
fn csi_letter_key(code: KeyCode) -> Option<char> {
    Some(match code {
        KeyCode::Up => 'A',
        KeyCode::Down => 'B',
        KeyCode::Right => 'C',
        KeyCode::Left => 'D',
        KeyCode::Home => 'H',
        KeyCode::End => 'F',
        KeyCode::F(1) => 'P',
        KeyCode::F(2) => 'Q',
        KeyCode::F(3) => 'R',
        KeyCode::F(4) => 'S',
        _ => return None,
    })
}

/// Leading number for keys encoded as `CSI <number> ; <mod> ~` when modified:
/// Insert/Delete, Page Up/Down, and F5–F12. The numbers mirror the plain
/// sequences in [`base_key_to_pty_bytes`].
fn csi_tilde_key(code: KeyCode) -> Option<u8> {
    Some(match code {
        KeyCode::Insert => 2,
        KeyCode::Delete => 3,
        KeyCode::PageUp => 5,
        KeyCode::PageDown => 6,
        KeyCode::F(5) => 15,
        KeyCode::F(6) => 17,
        KeyCode::F(7) => 18,
        KeyCode::F(8) => 19,
        KeyCode::F(9) => 20,
        KeyCode::F(10) => 21,
        KeyCode::F(11) => 23,
        KeyCode::F(12) => 24,
        _ => return None,
    })
}

fn base_key_to_pty_bytes(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    Some(match key.code {
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Left => cursor_key_bytes(application_cursor, 'D'),
        KeyCode::Right => cursor_key_bytes(application_cursor, 'C'),
        KeyCode::Up => cursor_key_bytes(application_cursor, 'A'),
        KeyCode::Down => cursor_key_bytes(application_cursor, 'B'),
        KeyCode::Home => cursor_key_bytes(application_cursor, 'H'),
        KeyCode::End => cursor_key_bytes(application_cursor, 'F'),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        // Never collapse Ctrl+Shift+C into ETX/Ctrl+C when enhanced keyboard
        // reporting lets us tell those keypresses apart.
        KeyCode::Char(_) if is_shifted_control_char(key, 'c') => return None,
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
        // Under the Kitty disambiguate protocol the host reports Shift combined
        // with Alt/Super as the unshifted base key plus a separate Shift bit
        // (e.g. Alt+Shift+h -> Char('h') + SHIFT|ALT) instead of folding Shift
        // into the glyph the way a legacy terminal does. Fold it back in here so
        // the shifted character reaches the PTY; otherwise the modifier is
        // dropped and Alt+Shift+h is indistinguishable from Alt+h to a legacy
        // app like vim. (Ctrl+Shift is handled above, where Shift never changes
        // the control byte.)
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            c.to_uppercase().to_string().into_bytes()
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

fn drain_agent_events(app: &mut App, backend: &mut impl AgentBackend) -> bool {
    let mut changed = false;
    for event in backend.drain_events() {
        changed = true;
        app.apply_agent_event(event);
    }
    changed
}

fn drain_mult_agent_status_events(app: &mut App) -> bool {
    let chats = app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id))
        .collect::<Vec<_>>();

    let mut changed = false;
    for chat in chats {
        if let Some(status) = read_mult_agent_status(&mult_agent_status_path(chat)) {
            changed |= app.mark_chat_status_by_id(chat, status);
        }
    }
    changed
}

/// Upper bound on the agent status file. It is a tiny JSON object; anything
/// larger is a bug or a hostile same-UID writer, and this read happens on the
/// render thread once per frame per chat, so it must never read unboundedly.
const MAX_STATUS_FILE_BYTES: u64 = 64 * 1024;

fn read_mult_agent_status(path: &Path) -> Option<ChatStatus> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW: never follow a symlink swapped in for the status file.
        // O_NONBLOCK: opening a FIFO or device must not stall the render thread.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }

    let file = options.open(path).ok()?;
    // Read regular files only; a swapped-in FIFO/socket/device is ignored.
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }

    let mut contents = String::new();
    file.take(MAX_STATUS_FILE_BYTES)
        .read_to_string(&mut contents)
        .ok()?;
    let record = serde_json::from_str::<MultAgentStatusRecord>(&contents).ok()?;
    mult_agent_status_to_chat_status(&record.status)
}

fn mult_agent_status_to_chat_status(status: &str) -> Option<ChatStatus> {
    match status {
        "idle" => Some(ChatStatus::Idle),
        "running" => Some(ChatStatus::Thinking),
        "waiting" => Some(ChatStatus::Waiting),
        "error" => Some(ChatStatus::Failed),
        "finished" => Some(ChatStatus::Done),
        _ => None,
    }
}

/// The agent backend a chat runs, looked up by chat id alone (the durable model
/// keys chats under workspaces, but PTY events only carry the chat id).
fn chat_agent_kind(app: &App, chat_id: model::ChatId) -> AgentKind {
    app.project
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.chats.iter())
        .find(|chat| chat.id == chat_id)
        .map(|chat| chat.agent)
        .unwrap_or_default()
}

fn mult_agent_status_path(chat: model::ChatId) -> PathBuf {
    let dir = ensure_mult_runtime_dir().unwrap_or_else(|_| mult_runtime_dir());
    dir.join(format!(
        "mult-agent-status-{}-{}.json",
        std::process::id(),
        chat.0
    ))
}

fn ensure_mult_runtime_dir() -> io::Result<PathBuf> {
    let dir = mult_runtime_dir();
    mult_protocol::ensure_private_dir(&dir)?;
    Ok(dir)
}

fn mult_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("mult-{}", current_euid())))
        .join("mult")
}

fn current_euid() -> u32 {
    unsafe { libc::geteuid() as u32 }
}

fn random_u64() -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u64::from_ne_bytes(bytes))
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
    fn process_agent_command_supports_basic_shell_quoting() {
        let command = parse_process_agent_command(
            "agent-cli --prompt 'hello world' \"two words\" escaped\\ space",
        )
        .expect("command parses");

        assert_eq!(command.program, "agent-cli");
        assert_eq!(
            command.args,
            vec!["--prompt", "hello world", "two words", "escaped space"]
        );
    }

    #[test]
    fn blank_or_unterminated_process_agent_command_is_ignored() {
        assert_eq!(parse_process_agent_command("   "), None);
        assert_eq!(parse_process_agent_command("agent 'unterminated"), None);
    }

    #[test]
    fn read_mult_agent_status_parses_a_small_status_file() {
        let path = unique_status_path("small");
        fs::write(&path, r#"{"status":"running"}"#).expect("write status");

        assert_eq!(read_mult_agent_status(&path), Some(ChatStatus::Thinking));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_mult_agent_status_caps_the_read_and_rejects_oversized_files() {
        let path = unique_status_path("huge");
        // Valid JSON as a whole, but far larger than the cap. Read in full it
        // would parse; truncated at the cap it cannot, so a bounded read rejects
        // it — proving the read never grows with the file.
        let padding = " ".repeat(MAX_STATUS_FILE_BYTES as usize + 1024);
        fs::write(&path, format!(r#"{{"status":"idle",{padding}"x":1}}"#)).expect("write status");

        assert_eq!(read_mult_agent_status(&path), None);

        let _ = fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn read_mult_agent_status_does_not_follow_symlinks() {
        let target = unique_status_path("symlink-target");
        fs::write(&target, r#"{"status":"idle"}"#).expect("write status");
        let link = unique_status_path("symlink-link");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        // O_NOFOLLOW means the symlink is not traversed, so nothing is read.
        assert_eq!(read_mult_agent_status(&link), None);

        let _ = fs::remove_file(&link);
        let _ = fs::remove_file(&target);
    }

    fn unique_status_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "mult-status-test-{label}-{}-{nanos}.json",
            std::process::id()
        ))
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

    #[test]
    fn pi_command_comes_from_config_with_default_fallback() {
        assert_eq!(
            pi_command(&Config {
                pi_agent_command: "pi -c".to_string(),
                ..Config::default()
            }),
            "pi -c"
        );
        assert_eq!(
            pi_command(&Config {
                pi_agent_command: "   ".to_string(),
                ..Config::default()
            }),
            "pi"
        );
    }

    #[test]
    fn claude_code_command_comes_from_config_with_default_fallback() {
        assert_eq!(
            claude_code_command(&Config {
                claude_code_command: "claude --resume".to_string(),
                ..Config::default()
            }),
            "claude --resume"
        );
        assert_eq!(
            claude_code_command(&Config {
                claude_code_command: "   ".to_string(),
                ..Config::default()
            }),
            "claude"
        );
    }

    #[test]
    fn pi_command_appends_mult_status_extension_when_available() {
        let command = pi_command_with_mult_status_extension(&Config {
            pi_agent_command: "pi --model test".to_string(),
            ..Config::default()
        });

        assert!(command.starts_with("pi --model test"));
        assert!(command.contains(" -e "));
        assert!(command.contains("mult-status-extension-"));
    }

    #[test]
    fn agent_command_routes_by_kind() {
        let config = Config {
            pi_agent_command: "pi".to_string(),
            claude_code_command: "claude --here".to_string(),
            ..Config::default()
        };

        // Pi takes the bundled status extension (`-e`); Claude Code takes a
        // generated hooks settings file (`--settings`). Neither borrows the
        // other's flag.
        let pi = agent_command(&config, AgentKind::Pi);
        assert!(pi.starts_with("pi"));
        assert!(pi.contains(" -e "));
        assert!(!pi.contains(" --settings "));

        let cc = agent_command(&config, AgentKind::ClaudeCode);
        assert!(cc.starts_with("claude --here"));
        assert!(cc.contains(" --settings "));
        assert!(!cc.contains(" -e "));
    }

    #[test]
    fn claude_code_command_appends_mult_status_hooks_when_available() {
        let command = claude_code_command_with_mult_status_hooks(&Config {
            claude_code_command: "claude --model test".to_string(),
            ..Config::default()
        });

        assert!(command.starts_with("claude --model test"));
        assert!(command.contains(" --settings "));
        assert!(command.contains("mult-claude-settings-"));
    }

    #[test]
    fn mult_claude_status_settings_json_maps_each_event() {
        let json = mult_claude_status_settings_json(Path::new("/run/mult/status.sh"));
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid settings json");

        // Each lifecycle event registers one matcher-less command hook that runs
        // the bundled script with the mult status for that event.
        for (event, status) in [
            ("SessionStart", "idle"),
            ("UserPromptSubmit", "running"),
            ("PreToolUse", "running"),
            ("Notification", "waiting"),
            ("Stop", "finished"),
        ] {
            let entry = &value["hooks"][event][0];
            assert_eq!(entry["matcher"], "");
            let command = entry["hooks"][0]["command"]
                .as_str()
                .expect("command is a string");
            assert_eq!(entry["hooks"][0]["type"], "command");
            assert!(
                command.starts_with("sh /run/mult/status.sh "),
                "unexpected command for {event}: {command}"
            );
            assert!(
                command.ends_with(&format!(" {status}")),
                "event {event} should map to status {status}, got {command}"
            );
        }
    }

    // The two halves of the feature must agree on the file schema: the bundled
    // shell script has to write exactly what `read_mult_agent_status` parses.
    #[cfg(unix)]
    #[test]
    fn bundled_claude_status_script_writes_a_status_mult_can_read() {
        let script = unique_status_path("cc-script");
        fs::write(&script, MULT_CLAUDE_STATUS_SCRIPT_SOURCE).expect("write script");
        let status_path = unique_status_path("cc-status");
        let _ = fs::remove_file(&status_path);

        let output = std::process::Command::new("sh")
            .arg(&script)
            .arg("running")
            .env(MULT_AGENT_STATUS_PATH_ENV, &status_path)
            .env(MULT_AGENT_CHAT_ID_ENV, "7")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run status script");
        assert!(output.status.success());

        // `running` round-trips through the file into mult's Thinking status.
        assert_eq!(
            read_mult_agent_status(&status_path),
            Some(ChatStatus::Thinking)
        );

        let _ = fs::remove_file(&script);
        let _ = fs::remove_file(&status_path);
    }

    #[test]
    fn shell_quote_handles_paths_with_spaces() {
        assert_eq!(shell_quote("/tmp/no-spaces.ts"), "/tmp/no-spaces.ts");
        assert_eq!(shell_quote("/tmp/has space.ts"), "'/tmp/has space.ts'");
        assert_eq!(shell_quote("/tmp/it's.ts"), "'/tmp/it'\\''s.ts'");
    }

    #[test]
    fn mult_agent_status_file_updates_chat_status() {
        // Startup no longer seeds agent chats, so add one explicitly for the
        // status-file test (the lib's test-only seed helper is not visible to
        // this bin-crate module).
        let mut state = model::ProjectState::default();
        let workspace = state.workspaces[0].id;
        state.add_chat(
            workspace,
            model::DEFAULT_AGENT_CHAT_TITLE.to_string(),
            ChatStatus::Idle,
            AgentKind::Pi,
        );
        let mut app = App::new(state);
        app.project.workspaces[0].chats[0].id = model::ChatId(9_001);
        let chat = app.project.workspaces[0].chats[0].id;
        let path = mult_agent_status_path(chat);
        let _ = fs::remove_file(&path);
        fs::write(&path, r#"{"version":1,"status":"finished"}"#).expect("write status file");

        drain_mult_agent_status_events(&mut app);

        assert_eq!(app.project.workspaces[0].chats[0].status, ChatStatus::Done);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ctrl_j_and_ctrl_k_navigate_selection() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(app.selected_index(), Some(1));
        assert_eq!(app.selected_item(), Some(app.nav_items()[1]));

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(app.selected_index(), Some(0));
        assert_eq!(app.selected_item(), Some(app.nav_items()[0]));
    }

    #[test]
    fn ctrl_p_opens_palette_and_ctrl_s_opens_search_for_selected_pane() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::CommandPalette(_))));
        app.cancel_prompt();

        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        let target = NavItem::Terminal {
            workspace,
            terminal,
        };
        app.select_item(target);
        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::Search(_))));
    }

    #[test]
    fn plain_keys_are_not_workspace_commands() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            frame_area,
        );
        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            frame_area,
        );

        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
        assert!(!app.should_quit);
        assert_eq!(app.prompt, None);
    }

    #[test]
    fn mouse_wheel_scrolls_output_under_cursor() {
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                mult::app::NavItem::Terminal { terminal, .. } => {
                    Some((index, PtyKey::Terminal(*terminal)))
                }
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let config = Config {
            mouse_capture: true,
            ..Config::default()
        };
        let frame_area = Rect::new(0, 0, 120, 40);
        let (_, output_area) = ui::selected_terminal_output_area(&app, frame_area)
            .expect("terminal selection has output area");

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        assert_eq!(
            pty_runtime.terminal_lines(terminal_id),
            vec!["one".to_string(), "two".to_string()]
        );

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        assert_eq!(
            pty_runtime.terminal_lines(terminal_id),
            vec!["four".to_string(), "five".to_string()]
        );
    }

    #[test]
    fn mouse_wheel_does_not_scroll_local_buffer_when_program_grabs_mouse() {
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                mult::app::NavItem::Terminal { terminal, .. } => {
                    Some((index, PtyKey::Terminal(*terminal)))
                }
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        // The program turns on mouse reporting: the wheel is now its input, so
        // our local scrollback must stay pinned to the bottom.
        pty_runtime.process_terminal_output(terminal_id, b"\x1b[?1000h\x1b[?1006h");
        let config = Config {
            mouse_capture: true,
            ..Config::default()
        };
        let frame_area = Rect::new(0, 0, 120, 40);
        let (_, output_area) = ui::selected_terminal_output_area(&app, frame_area)
            .expect("terminal selection has output area");

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );

        assert_eq!(
            pty_runtime
                .parser(terminal_id)
                .unwrap()
                .screen()
                .scrollback(),
            0
        );
        assert_eq!(
            pty_runtime.terminal_lines(terminal_id),
            vec!["four".to_string(), "five".to_string()]
        );
    }

    #[test]
    fn mouse_wheel_scroll_moves_text_selection_with_scrollback() {
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                mult::app::NavItem::Terminal { terminal, .. } => {
                    Some((index, PtyKey::Terminal(*terminal)))
                }
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 0, col: 2 });

        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let config = Config {
            mouse_capture: true,
            ..Config::default()
        };
        let frame_area = Rect::new(0, 0, 120, 40);
        let (_, output_area) = ui::selected_terminal_output_area(&app, frame_area)
            .expect("terminal selection has output area");

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        let selection = app
            .text_selection_for(terminal_id)
            .expect("selection follows scroll up");
        assert_eq!(selection.anchor.row, 3);
        assert_eq!(selection.focus.row, 3);

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        let selection = app
            .text_selection_for(terminal_id)
            .expect("selection follows scroll down");
        assert_eq!(selection.anchor.row, 0);
        assert_eq!(selection.focus.row, 0);
    }

    #[test]
    fn terminal_text_selection_extracts_visible_pane_text() {
        let terminal = PtyKey::Terminal(model::TerminalId(77));
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal, b"abc\r\ndef");

        let selection = TextSelection {
            terminal,
            anchor: SelectionCell { row: 0, col: 1 },
            focus: SelectionCell { row: 1, col: 0 },
            dragging: false,
        };

        assert_eq!(
            selected_text(&pty_runtime, selection).as_deref(),
            Some("bc\nd")
        );
    }

    #[test]
    fn wide_char_text_selection_extracts_expected_cells() {
        let terminal = PtyKey::Terminal(model::TerminalId(78));
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal, PtyDimensions { rows: 1, cols: 8 })
            .expect("resize parser");
        // 'a' at col 0; the wide '你' occupies cols 1-2 (glyph at 1, continuation
        // at 2); 'b' at col 3.
        pty_runtime.process_terminal_output(terminal, "a你b".as_bytes());

        let select = |start: u16, end: u16| {
            selected_text(
                &pty_runtime,
                TextSelection {
                    terminal,
                    anchor: SelectionCell { row: 0, col: start },
                    focus: SelectionCell { row: 0, col: end },
                    dragging: false,
                },
            )
        };

        assert_eq!(select(0, 3).as_deref(), Some("a你b"));
        assert_eq!(select(0, 0).as_deref(), Some("a"));
        assert_eq!(select(0, 1).as_deref(), Some("a你"));
        assert_eq!(select(1, 3).as_deref(), Some("你b"));
        assert_eq!(select(3, 3).as_deref(), Some("b"));
    }

    #[test]
    fn base64_encode_pads_clipboard_payloads() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn ctrl_keys_create_delete_and_quit() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(
                KeyCode::Char('Q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            frame_area,
        );
        assert!(!app.should_quit);

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(app.should_quit);
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        app.should_quit = false;
        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(app.prompt, None);
        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
    }

    #[test]
    fn ctrl_x_adds_a_claude_code_agent_chat() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let workspace = app.project.workspaces[0].id;
        assert!(app.project.workspaces[0].chats.is_empty());

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            frame_area,
        );

        // Ctrl+x adds and selects a chat backed by Claude Code, distinct from
        // the pi chat that Ctrl+a creates.
        assert_eq!(app.project.workspaces[0].chats.len(), 1);
        assert_eq!(
            app.project.workspaces[0].chats[0].agent,
            AgentKind::ClaudeCode
        );
        let chat = app.project.workspaces[0].chats[0].id;
        assert_eq!(app.selected_item(), Some(NavItem::Chat { workspace, chat }));
    }

    #[test]
    fn ctrl_c_is_not_a_command_terminal_shortcut() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            frame_area,
        );

        assert_eq!(app.prompt, None);
        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
    }

    #[test]
    fn ctrl_shift_c_is_copy_shortcut_not_pty_interrupt() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let key = KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );

        assert!(handle_control_key(
            &mut app,
            &mut pty_runtime,
            &config,
            key,
            frame_area,
        ));
        assert!(key_to_pty_bytes(key).is_empty());
        assert!(key_to_pty_bytes(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .is_empty());
    }

    #[test]
    fn ctrl_f_opens_workspace_prompt() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::OpenWorkspace(_))));
    }

    #[test]
    fn open_workspace_prompt_ctrl_j_and_ctrl_k_select_matches() {
        let mut app = App::default();
        let config = Config {
            projects: vec![
                mult::config::ConfiguredProject {
                    name: "first".to_string(),
                    path: "/tmp/first".into(),
                },
                mult::config::ConfiguredProject {
                    name: "second".to_string(),
                    path: "/tmp/second".into(),
                },
            ],
            ..Config::default()
        };

        app.begin_open_workspace(&config.projects);
        handle_open_workspace_key(
            &mut app,
            &config,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            app.prompt,
            Some(Prompt::OpenWorkspace(ref prompt)) if prompt.selected == 1
        ));

        handle_open_workspace_key(
            &mut app,
            &config,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            app.prompt,
            Some(Prompt::OpenWorkspace(ref prompt)) if prompt.selected == 0
        ));
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
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            b"\x1b".to_vec()
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

    #[test]
    fn alt_shift_letters_fold_shift_into_uppercase() {
        // Regression: under the Kitty disambiguate protocol crossterm reports
        // Alt+Shift+h as Char('h') + SHIFT|ALT (the unshifted base key). Shift
        // must survive as an uppercase glyph so the PTY sees `ESC H` (<M-H>), not
        // `ESC h` (<M-h>) — otherwise Alt+Shift+h/j/k/l collapse onto
        // Alt+h/j/k/l inside vim.
        for (lower, upper) in [('h', 'H'), ('j', 'J'), ('k', 'K'), ('l', 'L')] {
            assert_eq!(
                key_to_pty_bytes(KeyEvent::new(
                    KeyCode::Char(lower),
                    KeyModifiers::ALT | KeyModifiers::SHIFT,
                )),
                vec![0x1b, upper as u8],
                "Alt+Shift+{lower} must encode as ESC {upper}",
            );
        }
    }

    #[test]
    fn alt_arrow_keys_use_csi_modifier_encoding() {
        // Regression: Alt+Arrow must move the cursor via `CSI 1 ; 3 <dir>`, not
        // arrive as a doubled-ESC sequence that the PTY renders as characters.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            b"\x1b[1;3D".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
            b"\x1b[1;3C".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
            b"\x1b[1;3A".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
            b"\x1b[1;3B".to_vec()
        );
    }

    #[test]
    fn ctrl_and_shift_arrows_use_csi_modifier_encoding() {
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            b"\x1b[1;5D".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT)),
            b"\x1b[1;2C".to_vec()
        );
        // Combined modifiers follow the xterm bitmask: 1 + shift + alt*2 + ctrl*4.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            b"\x1b[1;7A".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            )),
            b"\x1b[1;8F".to_vec()
        );
    }

    #[test]
    fn modified_home_paging_and_function_keys_encode_modifiers() {
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL)),
            b"\x1b[1;5H".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL)),
            b"\x1b[3;5~".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT)),
            b"\x1b[5;2~".to_vec()
        );
        // F1–F4 switch from SS3 to CSI form once modified; F5+ keep the tilde form.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::F(1), KeyModifiers::SHIFT)),
            b"\x1b[1;2P".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::F(5), KeyModifiers::CONTROL)),
            b"\x1b[15;5~".to_vec()
        );
    }

    #[test]
    fn unmodified_navigation_keys_keep_plain_sequences() {
        // Without a modifier there are no CSI parameters, matching every VT100 app.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            b"\x1b[D".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
            b"\x1b[3~".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            b"\x1bOP".to_vec()
        );
    }

    #[test]
    fn alt_simple_keys_still_use_meta_escape_prefix() {
        // The meta convention stays correct for printable characters and keys
        // whose base encoding is a single control byte.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
            b"\x1bb".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
            b"\x1b\x7f".to_vec()
        );
    }

    #[test]
    fn application_cursor_mode_uses_ss3_for_unmodified_cursor_keys() {
        // DECCKM: full-screen apps (vim, less, fzf) expect SS3 (`ESC O <dir>`)
        // arrows rather than the CSI (`ESC [ <dir>`) form used by the shell.
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true),
            b"\x1bOA".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), true),
            b"\x1bOD".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), true),
            b"\x1bOH".to_vec()
        );
    }

    #[test]
    fn application_cursor_mode_keeps_csi_for_modified_and_non_cursor_keys() {
        // A held modifier always selects the CSI form, even under DECCKM.
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), true),
            b"\x1b[1;3A".to_vec()
        );
        // Paging keys are not cursor keys, so DECCKM leaves them untouched.
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), true),
            b"\x1b[6~".to_vec()
        );
    }
}
