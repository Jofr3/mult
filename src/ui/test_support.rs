//! Shared helpers for the renderer's tests: draw a frame into a `TestBackend`
//! and read it back.

use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

use ratatui::style::{Color, Modifier};

use crate::{
    app::{App, NavItem},
    config,
    model::{AgentKind, ChatStatus, ProjectState, PtyKey},
    pty::PtyRuntime,
};

use crate::layout::AppLayout;

use super::{draw, theme::Palette};

pub(super) fn buffer_text(backend: &TestBackend, x: u16, y: u16, width: u16) -> String {
    let mut text = String::new();
    for offset in 0..width {
        text.push_str(
            backend
                .buffer()
                .cell((x + offset, y))
                .expect("cell is in bounds")
                .symbol(),
        );
    }
    text
}
pub(super) fn draw_text(app: &App, pty_runtime: &PtyRuntime, width: u16, height: u16) -> String {
    draw_text_with_config(app, pty_runtime, &config::Config::default(), width, height)
}

pub(super) fn draw_text_with_config(
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
    width: u16,
    height: u16,
) -> String {
    format!(
        "{:?}",
        draw_buffer_with_config(app, pty_runtime, config, width, height)
    )
}

/// The whole rendered buffer, for tests that read cells rather than text.
pub(super) fn draw_buffer_with_config(
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
    width: u16,
    height: u16,
) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("create test terminal");
    terminal
        .draw(|frame| {
            let layout = AppLayout::compute(app, frame.area());
            draw(frame, &layout, app, pty_runtime, config)
        })
        .expect("draw app");
    terminal.backend().buffer().clone()
}
pub(super) fn test_palette() -> Palette {
    Palette::from_colors(config::Config::default().colors())
}

/// A fixed project for the layout snapshots.
///
/// Everything the frame can show is pinned here — literal workspace, chat
/// and terminal names, and a stubbed git branch instead of a probe against
/// whatever repository the test happens to run in — so a snapshot diff means
/// the layout changed and nothing else.
pub(super) fn snapshot_app() -> App {
    let mut project = ProjectState::two_workspaces();
    project.workspaces.clear();
    let workspace = project.add_workspace("orbit".to_string(), None);
    project.add_chat(
        workspace,
        "agent".to_string(),
        ChatStatus::Idle,
        AgentKind::Pi,
    );
    project.add_terminal(workspace, "shell".to_string(), false);

    let mut app = App::new(project);
    app.replace_workspace_git_branches([(workspace, Some("main".to_string()))]);
    app
}

/// Select the fixture's terminal.
///
/// Nav order puts chats before terminals, so the default selection is the
/// chat — whose blank-pane hint prints `config::config_path()`, i.e. the
/// running user's home directory. The terminal pane has no such dependency.
pub(super) fn select_snapshot_terminal(app: &mut App) -> PtyKey {
    let workspace = app.project.workspaces[0].id;
    let terminal = app.project.workspaces[0].terminals[0].id;
    app.select_item(NavItem::Terminal {
        workspace,
        terminal,
    });
    PtyKey::Terminal(terminal)
}

/// One character naming a cell's background, so the snapshot pins pane
/// boundaries, the list highlight and the selection — none of which a grid
/// of symbols can show.
pub(super) fn background_symbol(background: Color, palette: Palette) -> char {
    match background {
        color if color == palette.foam => '*',
        color if color == palette.highlight_med => '#',
        color if color == palette.cursor => '@',
        color if color == palette.base => ':',
        color if color == palette.nc => '.',
        Color::Reset => 'r',
        _ => '?',
    }
}

/// The whole rendered buffer, as a grid of symbols followed by a grid of
/// backgrounds. Rows are bracketed so trailing blanks show up in a diff.
pub(super) fn buffer_grid(backend: &TestBackend) -> String {
    let palette = test_palette();
    let buffer = backend.buffer();
    let area = buffer.area();
    let mut grid = String::from("symbols:\n");
    for y in area.top()..area.bottom() {
        grid.push('|');
        for x in area.left()..area.right() {
            grid.push_str(buffer.cell((x, y)).expect("cell is in bounds").symbol());
        }
        grid.push_str("|\n");
    }

    grid.push_str("\nbackgrounds (. nc  : base  # highlight  * selection  @ cursor  r reset):\n");
    for y in area.top()..area.bottom() {
        grid.push('|');
        for x in area.left()..area.right() {
            let background = buffer.cell((x, y)).expect("cell is in bounds").bg;
            grid.push(background_symbol(background, palette));
        }
        grid.push_str("|\n");
    }
    grid
}

/// One character naming a cell's attributes, for the `NO_COLOR` grid.
fn attribute_symbol(modifier: Modifier) -> char {
    match modifier {
        m if m.contains(Modifier::REVERSED) => 'R',
        m if m.contains(Modifier::BOLD) => 'B',
        m if m.contains(Modifier::DIM) => 'D',
        m if m.contains(Modifier::UNDERLINED) => 'U',
        m if m.is_empty() => '-',
        _ => '?',
    }
}

/// The `NO_COLOR` frame as a grid of symbols followed by a grid of attributes.
///
/// With every colour `Color::Reset`, a background grid says nothing at all —
/// the attributes *are* the affordance, so they are what has to be pinned (F15).
pub(super) fn monochrome_grid(buffer: &Buffer) -> String {
    let area = *buffer.area();
    let mut grid = String::from("symbols:\n");
    for y in area.top()..area.bottom() {
        grid.push('|');
        for x in area.left()..area.right() {
            grid.push_str(buffer.cell((x, y)).expect("cell is in bounds").symbol());
        }
        grid.push_str("|\n");
    }

    grid.push_str("\nattributes (R reversed  B bold  D dim  U underlined  - none):\n");
    for y in area.top()..area.bottom() {
        grid.push('|');
        for x in area.left()..area.right() {
            let cell = buffer.cell((x, y)).expect("cell is in bounds");
            grid.push(attribute_symbol(cell.modifier));
        }
        grid.push_str("|\n");
    }
    grid
}

pub(super) fn draw_grid(app: &App, pty_runtime: &PtyRuntime, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("create test terminal");
    terminal
        .draw(|frame| {
            let layout = AppLayout::compute(app, frame.area());
            draw(frame, &layout, app, pty_runtime, &config::Config::default())
        })
        .expect("draw app");
    buffer_grid(terminal.backend())
}
