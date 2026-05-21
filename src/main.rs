use std::{io, time::Duration};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
};
use mult::{
    agent::{
        self, AgentBackend, AgentEvent, NoopAgentBackend, ProcessAgentBackend, ProcessAgentCommand,
    },
    app::{
        chat_agent_terminal_id, chat_id_from_agent_terminal_id, App, CommandAction, FocusMode,
        Mode, Prompt,
    },
    config::{self, Config},
    model::{self, ChatStatus, TerminalId, TerminalLaunch, TerminalStatus},
    pty::{PtyDimensions, PtyEvent, PtyRuntime, PtySpawn},
    storage, ui,
};
use ratatui::{layout::Rect, DefaultTerminal};

fn main() -> io::Result<()> {
    let project = storage::load_or_default()?;
    let config = config::load_or_default()?;
    let mut terminal = ratatui::init();
    if let Err(error) = execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste) {
        ratatui::restore();
        return Err(error);
    }

    let result = run(&mut terminal, App::new(project), config);
    let mouse_result = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
    result.and(mouse_result)
}

const AGENT_CMD_ENV: &str = "MULT_AGENT_CMD";
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(1);
const READY_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(0);
const MOUSE_SCROLL_ROWS: usize = 3;

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
    let mut normal_prefix = None;
    let size = terminal.size()?;
    let mut frame_area = Rect::new(0, 0, size.width, size.height);
    restore_persisted_sessions(&mut app, &mut pty_runtime, &config, frame_area);

    while !app.should_quit {
        drain_pty_events(&mut app, &mut pty_runtime);
        drain_agent_events(&mut app, &mut agent_backend);
        save_if_dirty(&mut app)?;
        resize_visible_terminal(&mut app, &mut pty_runtime, frame_area);
        resize_visible_chat_agent(&mut app, &mut pty_runtime, frame_area);
        auto_start_selected_terminal(&mut app, &mut pty_runtime, &config, frame_area);
        auto_start_selected_chat_agent(&mut app, &mut pty_runtime, &config, frame_area);
        frame_area = terminal.draw(|frame| ui::draw(frame, &app, &config))?.area;

        if event::poll(EVENT_POLL_INTERVAL)? {
            handle_event(
                &mut app,
                &mut pty_runtime,
                &config,
                &mut normal_prefix,
                event::read()?,
                frame_area,
            );
            while !app.should_quit && event::poll(READY_EVENT_POLL_INTERVAL)? {
                handle_event(
                    &mut app,
                    &mut pty_runtime,
                    &config,
                    &mut normal_prefix,
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
        start_terminal(app, pty_runtime, frame_area, workspace, terminal);
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
    normal_prefix: &mut Option<NormalPrefix>,
    event: Event,
    frame_area: Rect,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, pty_runtime, config, normal_prefix, key, frame_area);
        }
        Event::Mouse(mouse) => handle_mouse(app, pty_runtime, normal_prefix, mouse, frame_area),
        Event::Paste(text) => handle_paste(app, pty_runtime, normal_prefix, text),
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NormalPrefix {
    Delete,
    New,
}

fn handle_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    normal_prefix: &mut Option<NormalPrefix>,
    key: KeyEvent,
    frame_area: Rect,
) {
    match (&app.prompt, app.mode) {
        (Some(Prompt::OpenWorkspace(_)), _) => {
            *normal_prefix = None;
            handle_open_workspace_key(app, key);
        }
        (Some(Prompt::NewTerminalCommand(_)), _) => {
            *normal_prefix = None;
            handle_terminal_command_key(app, key);
        }
        (Some(Prompt::CommandPalette(_)), _) => {
            *normal_prefix = None;
            handle_command_palette_key(app, pty_runtime, config, key, frame_area);
        }
        (Some(Prompt::Search(_)), _) => {
            *normal_prefix = None;
            handle_search_key(app, key);
        }
        (None, Mode::Input(_)) => {
            *normal_prefix = None;
            handle_pty_input_key(app, pty_runtime, key);
        }
        (None, Mode::Normal) => {
            handle_normal_key(app, pty_runtime, config, normal_prefix, key, frame_area);
        }
    }
}

fn handle_mouse(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    normal_prefix: &mut Option<NormalPrefix>,
    mouse: MouseEvent,
    frame_area: Rect,
) {
    if app.is_prompt_active() {
        return;
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            *normal_prefix = None;
            scroll_output_at_mouse(app, pty_runtime, frame_area, mouse, ScrollDirection::Up);
        }
        MouseEventKind::ScrollDown => {
            *normal_prefix = None;
            scroll_output_at_mouse(app, pty_runtime, frame_area, mouse, ScrollDirection::Down);
        }
        _ => {}
    }
}

fn handle_paste(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    normal_prefix: &mut Option<NormalPrefix>,
    text: String,
) {
    *normal_prefix = None;
    if app.is_prompt_active() {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            app.push_prompt_char(ch);
        }
        return;
    }

    if !matches!(app.mode, Mode::Input(_)) {
        return;
    }

    let Some(terminal_id) = app.pty_input_target() else {
        app.end_pty_input();
        return;
    };

    match pty_runtime.send_paste(terminal_id, &text) {
        Ok(true) => {}
        Ok(false) => {
            app.append_terminal_system_line(terminal_id, "PTY is not running");
            app.end_pty_input();
        }
        Err(error) => {
            app.append_terminal_system_line(terminal_id, format!("failed to paste: {error}"));
            app.end_pty_input();
        }
    }
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
    let Some(terminal) = output_terminal_at(app, frame_area, mouse.column, mouse.row) else {
        return false;
    };

    match direction {
        ScrollDirection::Up => {
            scroll_terminal_output_up(app, pty_runtime, terminal, MOUSE_SCROLL_ROWS)
        }
        ScrollDirection::Down => {
            scroll_terminal_output_down(app, pty_runtime, terminal, MOUSE_SCROLL_ROWS)
        }
    }
}

fn output_terminal_at(app: &App, frame_area: Rect, column: u16, row: u16) -> Option<TerminalId> {
    if let Some((terminal, area)) = ui::selected_terminal_output_area(app, frame_area) {
        if rect_contains(area, column, row) {
            return Some(terminal);
        }
    }

    if let Some((chat, area)) = ui::selected_chat_agent_output_area(app, frame_area) {
        if rect_contains(area, column, row) {
            return Some(chat_agent_terminal_id(chat));
        }
    }

    None
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn handle_normal_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    normal_prefix: &mut Option<NormalPrefix>,
    key: KeyEvent,
    frame_area: Rect,
) {
    if let Some(prefix) = normal_prefix.take() {
        handle_normal_prefix_key(app, pty_runtime, config, prefix, key, frame_area);
        return;
    }

    match key.code {
        KeyCode::Char('q') => app.quit(),
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => app.clear_search(),
        KeyCode::Char(':') => app.begin_command_palette(),
        KeyCode::Char('/') => {
            app.begin_search();
        }
        KeyCode::Esc => app.focus_sidebar(),
        KeyCode::Enter if app.focus == FocusMode::Sidebar => {
            app.focus_selected_main();
        }
        KeyCode::Char('j') | KeyCode::Down if app.focus == FocusMode::Sidebar => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up if app.focus == FocusMode::Sidebar => {
            app.select_previous();
        }
        KeyCode::Char('k') | KeyCode::Up if output_pane_is_focused(app) => {
            scroll_selected_output_up(app, pty_runtime, 1);
        }
        KeyCode::Char('j') | KeyCode::Down if output_pane_is_focused(app) => {
            scroll_selected_output_down(app, pty_runtime, 1);
        }
        KeyCode::PageUp if output_pane_is_focused(app) => {
            scroll_selected_output_up(app, pty_runtime, scroll_page_rows(app, frame_area));
        }
        KeyCode::PageDown if output_pane_is_focused(app) => {
            scroll_selected_output_down(app, pty_runtime, scroll_page_rows(app, frame_area));
        }
        KeyCode::Home if output_pane_is_focused(app) => {
            scroll_selected_output_to_top(app, pty_runtime);
        }
        KeyCode::End if output_pane_is_focused(app) => {
            scroll_selected_output_to_bottom(app, pty_runtime);
        }
        KeyCode::Char('n') => *normal_prefix = Some(NormalPrefix::New),
        KeyCode::Char('d') => *normal_prefix = Some(NormalPrefix::Delete),
        KeyCode::Char('i') => focus_selected_input(app, pty_runtime, config, frame_area),
        _ => {}
    }
}

fn output_pane_is_focused(app: &App) -> bool {
    matches!(app.focus, FocusMode::Chat | FocusMode::Terminal)
        && app.selected_output_terminal_id().is_some()
}

fn scroll_selected_output_up(app: &mut App, pty_runtime: &mut PtyRuntime, rows: usize) -> bool {
    let Some(terminal) = app.selected_output_terminal_id() else {
        return false;
    };
    scroll_terminal_output_up(app, pty_runtime, terminal, rows)
}

fn scroll_selected_output_down(app: &mut App, pty_runtime: &mut PtyRuntime, rows: usize) -> bool {
    let Some(terminal) = app.selected_output_terminal_id() else {
        return false;
    };
    scroll_terminal_output_down(app, pty_runtime, terminal, rows)
}

fn scroll_selected_output_to_top(app: &mut App, pty_runtime: &mut PtyRuntime) -> bool {
    let Some(terminal) = app.selected_output_terminal_id() else {
        return false;
    };
    if pty_runtime.is_running(terminal) {
        return pty_runtime.scroll_to_top(terminal).unwrap_or(false);
    }

    app.scroll_terminal_output_to_top(terminal)
}

fn scroll_selected_output_to_bottom(app: &mut App, pty_runtime: &mut PtyRuntime) -> bool {
    let Some(terminal) = app.selected_output_terminal_id() else {
        return false;
    };
    if pty_runtime.is_running(terminal) {
        return pty_runtime.scroll_to_bottom(terminal).unwrap_or(false);
    }

    app.scroll_terminal_output_to_bottom(terminal)
}

fn scroll_terminal_output_up(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    terminal: TerminalId,
    rows: usize,
) -> bool {
    if pty_runtime.is_running(terminal) {
        return pty_runtime.scroll_up(terminal, rows).unwrap_or(false);
    }

    app.scroll_terminal_output_up(terminal, rows)
}

fn scroll_terminal_output_down(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    terminal: TerminalId,
    rows: usize,
) -> bool {
    if pty_runtime.is_running(terminal) {
        return pty_runtime.scroll_down(terminal, rows).unwrap_or(false);
    }

    app.scroll_terminal_output_down(terminal, rows)
}

fn scroll_page_rows(app: &App, frame_area: Rect) -> usize {
    let height = ui::selected_terminal_output_area(app, frame_area)
        .map(|(_, area)| area.height)
        .or_else(|| {
            ui::selected_chat_agent_output_area(app, frame_area).map(|(_, area)| area.height)
        })
        .unwrap_or(1);
    usize::from(height.saturating_sub(1).max(1))
}

fn handle_normal_prefix_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    prefix: NormalPrefix,
    key: KeyEvent,
    frame_area: Rect,
) {
    match (prefix, key.code) {
        (NormalPrefix::Delete, KeyCode::Char('d')) => delete_selected_now(app, pty_runtime),
        (NormalPrefix::New, KeyCode::Char('a')) => {
            add_agent_to_selected_workspace(app, pty_runtime, config, frame_area);
        }
        (NormalPrefix::New, KeyCode::Char('t')) => app.add_terminal_to_selected_workspace(),
        (NormalPrefix::New, KeyCode::Char('c')) => {
            app.begin_new_terminal_command();
        }
        (NormalPrefix::New, KeyCode::Char('w')) => app.begin_open_workspace(),
        (_, KeyCode::Esc) => {}
        _ => {}
    }
}

fn add_agent_to_selected_workspace(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    if let Some((workspace, chat)) = app.add_chat_to_selected_workspace_and_return() {
        start_or_focus_chat_agent(app, pty_runtime, config, frame_area, workspace, chat, true);
    }
}

fn delete_selected_now(app: &mut App, pty_runtime: &mut PtyRuntime) {
    for terminal in app.delete_selected_immediately() {
        let _ = pty_runtime.stop(terminal);
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
        KeyCode::Backspace => app.pop_prompt_char(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_prompt(),
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
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_prompt(),
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
            add_agent_to_selected_workspace(app, pty_runtime, config, frame_area);
        }
        CommandAction::AddShellTerminal => app.add_terminal_to_selected_workspace(),
        CommandAction::AddCommandTerminal => {
            app.begin_new_terminal_command();
        }
        CommandAction::OpenWorkspace => app.begin_open_workspace(),
        CommandAction::DeleteSelected => delete_selected_now(app, pty_runtime),
        CommandAction::SearchSelectedPane => {
            app.begin_search();
        }
        CommandAction::ClearSearch => app.clear_search(),
        CommandAction::Quit => app.quit(),
    }
}

fn start_selected_terminal(app: &mut App, pty_runtime: &mut PtyRuntime, frame_area: Rect) {
    let Some((workspace_id, terminal_id)) = app.selected_terminal_id() else {
        return;
    };

    start_terminal(app, pty_runtime, frame_area, workspace_id, terminal_id);
}

fn start_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    frame_area: Rect,
    workspace_id: model::WorkspaceId,
    terminal_id: model::TerminalId,
) -> bool {
    if pty_runtime.is_running(terminal_id) {
        app.append_terminal_system_line(terminal_id, "PTY already running");
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
            true
        }
        Err(error) => {
            app.append_terminal_system_line(
                terminal_id,
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
    let chat_name = workspace
        .chats
        .iter()
        .find(|chat| chat.id == chat_id)
        .map(|chat| chat.name.clone())
        .unwrap_or_else(|| format!("chat {}", chat_id.0));
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
                format!("failed to start pi agent for `{chat_name}`: {error}"),
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
    if !config.auto_start_terminals || !matches!(app.mode, Mode::Normal) || app.is_prompt_active() {
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
    if !config.auto_start_pi_agent || !matches!(app.mode, Mode::Normal) || app.is_prompt_active() {
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
    app.terminal_output_is_blank(terminal_id)
}

fn pi_command(config: &Config) -> String {
    let command = config.pi_agent_command.trim();
    if command.is_empty() {
        "pi".to_string()
    } else {
        command.to_string()
    }
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
        start_or_focus_selected_terminal(app, pty_runtime, frame_area);
    }
}

fn start_or_focus_selected_terminal(app: &mut App, pty_runtime: &mut PtyRuntime, frame_area: Rect) {
    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return;
    };

    if !pty_runtime.is_running(terminal_id) {
        start_selected_terminal(app, pty_runtime, frame_area);
    }

    if pty_runtime.is_running(terminal_id) {
        app.begin_terminal_input();
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
    terminal_id: model::TerminalId,
) -> Option<PtyDimensions> {
    ui::selected_terminal_output_area(app, frame_area)
        .filter(|(selected_terminal, _)| *selected_terminal == terminal_id)
        .map(|(_, area)| pty_dimensions_from_area(area))
}

fn selected_chat_agent_dimensions(
    app: &App,
    frame_area: Rect,
    chat_id: model::ChatId,
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
            PtyEvent::Snapshot { terminal, snapshot } => {
                app.set_terminal_snapshot(terminal, snapshot)
            }
            PtyEvent::Update { terminal, update } => app.apply_terminal_update(terminal, update),
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
                    let exit_message = format!("pi agent exited: {}", status.label());
                    app.append_terminal_system_line(terminal, exit_message.as_str());
                } else {
                    app.mark_terminal_stopped(terminal);
                    if app.terminal_input_target() == Some(terminal) {
                        app.end_terminal_input();
                    }
                    let exit_message = format!("PTY exited: {}", status.label());
                    app.append_terminal_system_line(terminal, exit_message.as_str());
                }
            }
            PtyEvent::Error { terminal, message } => {
                app.append_terminal_system_line(terminal, message.as_str());
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
                colorscheme: Default::default(),
            }),
            "pi -c"
        );
        assert_eq!(
            pi_command(&Config {
                pi_agent_command: "   ".to_string(),
                auto_start_pi_agent: false,
                auto_start_terminals: false,
                colorscheme: Default::default(),
            }),
            "pi"
        );
    }

    #[test]
    fn h_and_l_do_not_move_focus_in_normal_mode() {
        let mut app = App::default();
        app.selected = 1;
        let mut pty_runtime = PtyRuntime::default();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        let mut normal_prefix = None;
        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.focus, FocusMode::Sidebar);

        assert!(app.focus_selected_main());
        assert_eq!(app.focus, FocusMode::Chat);
        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.focus, FocusMode::Chat);
    }

    #[test]
    fn removed_focus_keys_do_not_move_focus() {
        let mut app = App::default();
        app.selected = 1;
        let mut pty_runtime = PtyRuntime::default();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut normal_prefix = None;

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.focus, FocusMode::Sidebar);

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.focus, FocusMode::Sidebar);

        assert!(app.focus_selected_main());
        assert_eq!(app.focus, FocusMode::Chat);
        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.focus, FocusMode::Chat);

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.focus, FocusMode::Sidebar);
    }

    #[test]
    fn normal_mode_scroll_keys_scroll_focused_output_pane() {
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                mult::app::NavItem::Terminal { terminal, .. } => Some((index, *terminal)),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.selected = selected;
        assert!(app.focus_selected_main());
        app.resize_terminal_buffer(terminal_id, 2, 8);
        app.append_terminal_output(terminal_id, "one\r\ntwo\r\nthree");
        let mut pty_runtime = PtyRuntime::default();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut normal_prefix = None;

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(
            app.terminal_lines(terminal_id),
            vec!["one".to_string(), "two".to_string()]
        );

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(
            app.terminal_lines(terminal_id),
            vec!["two".to_string(), "three".to_string()]
        );
    }

    #[test]
    fn mouse_wheel_scrolls_output_under_cursor() {
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                mult::app::NavItem::Terminal { terminal, .. } => Some((index, *terminal)),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.selected = selected;
        app.resize_terminal_buffer(terminal_id, 2, 8);
        app.append_terminal_output(terminal_id, "one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let mut pty_runtime = PtyRuntime::default();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let (_, output_area) = ui::selected_terminal_output_area(&app, frame_area)
            .expect("terminal selection has output area");
        let mut normal_prefix = None;

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        assert_eq!(
            app.terminal_lines(terminal_id),
            vec!["one".to_string(), "two".to_string()]
        );

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        assert_eq!(
            app.terminal_lines(terminal_id),
            vec!["four".to_string(), "five".to_string()]
        );
    }

    #[test]
    fn normal_prefix_keys_create_and_delete_without_confirmation() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::default();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut normal_prefix = None;
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            frame_area,
        );
        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt, None);
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt, None);
        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
    }

    #[test]
    fn normal_prefix_keys_open_prompts() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::default();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut normal_prefix = None;

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            frame_area,
        );
        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::NewTerminalCommand(_))));
        app.cancel_prompt();

        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            frame_area,
        );
        handle_normal_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &mut normal_prefix,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::OpenWorkspace(_))));
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
