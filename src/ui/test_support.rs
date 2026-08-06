//! Fixtures shared by the `ui` submodule tests: rendering an `App` into a
//! `TestBackend`, and reading text back out of the resulting buffer.

use ratatui::{layout::Rect, style::Style};

use crate::{
    app::{App, NavItem},
    config::{self, ColorSchemeConfig},
    model::PtyKey,
    pty::PtyRuntime,
};

use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

use super::*;
use crate::ui::theme::Palette;

/// [`crate::ui::draw`] with the layout resolved from the frame, which is what
/// the event loop does for the frame it is about to paint.
pub(super) fn draw_app(
    frame: &mut ratatui::Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
) {
    let layout = crate::layout::AppLayout::compute(app, frame.area());
    super::draw(frame, app, pty_runtime, config, layout);
}

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

fn draw_text_with_config(
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
    width: u16,
    height: u16,
) -> String {
    format!(
        "{:?}",
        render_buffer(app, pty_runtime, config, width, height)
    )
}

pub(super) fn render_buffer(
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
    width: u16,
    height: u16,
) -> Buffer {
    render_buffer_with_palette(app, pty_runtime, config, config.palette(), width, height)
}

/// `NO_COLOR` is a process global that no test may mutate (G7), so the
/// monochrome frames drive the palette seam instead.
pub(super) fn render_buffer_with_palette(
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
    palette: Palette,
    width: u16,
    height: u16,
) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("create test terminal");
    terminal
        .draw(|frame| {
            let layout = crate::layout::AppLayout::compute(app, frame.area());
            draw_with_palette(frame, app, pty_runtime, config, palette, layout)
        })
        .expect("draw app");
    terminal.backend().buffer().clone()
}

/// Render a whole `TestBackend` buffer as a snapshot: the glyph grid, a
/// parallel grid of per-cell style keys, and a legend resolving them. The
/// style grid is the point — `.contains(…)` assertions cannot see a sidebar
/// that changed width, a header that moved, or a selection that stopped
/// painting, and all three are pane-background changes before they are text
/// changes.
///
/// Rows are bracketed with `|` so trailing blanks are visible. A wide glyph
/// leaves its successor cell's symbol empty (ratatui's own convention), so
/// the symbol row stays as wide as the terminal while the style row always
/// has exactly one key per cell.
pub(super) fn buffer_snapshot(buffer: &Buffer) -> String {
    let area = buffer.area();
    let mut legend: Vec<Style> = Vec::new();
    let mut symbol_rows = Vec::new();
    let mut style_rows = Vec::new();

    for y in area.top()..area.bottom() {
        let mut symbols = String::new();
        let mut styles = String::new();
        for x in area.left()..area.right() {
            let cell = buffer.cell((x, y)).expect("cell is in bounds");
            symbols.push_str(cell.symbol());
            let style = cell.style();
            let key = legend.iter().position(|known| *known == style);
            styles.push(legend_key(key.unwrap_or_else(|| {
                legend.push(style);
                legend.len() - 1
            })));
        }
        symbol_rows.push(symbols);
        style_rows.push(styles);
    }

    let mut snapshot = format!("size: {}x{}\n\nsymbols:\n", area.width, area.height);
    for row in &symbol_rows {
        snapshot.push_str(&format!("|{row}|\n"));
    }
    snapshot.push_str("\nstyles:\n");
    for row in &style_rows {
        snapshot.push_str(&format!("|{row}|\n"));
    }
    snapshot.push_str("\nlegend:\n");
    for (index, style) in legend.iter().enumerate() {
        snapshot.push_str(&format!("  {} = {style:?}\n", legend_key(index)));
    }
    snapshot
}

fn legend_key(index: usize) -> char {
    const KEYS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    KEYS.get(index).map_or('?', |key| char::from(*key))
}

/// The seed app with a terminal selected and `output` fed to a parser sized
/// to the visible output area, so terminal snapshots do not depend on how
/// the runtime happened to size the pane.
pub(super) fn terminal_app_with_output(
    frame_area: Rect,
    output: &[u8],
) -> (App, PtyRuntime, PtyKey) {
    let mut app = App::default();
    let selected = app
        .nav_items()
        .iter()
        .position(|item| matches!(item, NavItem::Terminal { .. }))
        .expect("seed state has a terminal");
    app.select_nav_index(selected);
    let (terminal_id, output_area) = crate::layout::AppLayout::compute(&app, frame_area)
        .selected_terminal_output(&app)
        .expect("terminal has an output area");
    let terminal_id = PtyKey::Terminal(terminal_id);

    let mut pty_runtime = PtyRuntime::new_offline();
    pty_runtime
        .resize(
            terminal_id,
            crate::pty::PtyDimensions {
                rows: output_area.height,
                cols: output_area.width,
            },
        )
        .expect("resize parser");
    pty_runtime.process_terminal_output(terminal_id, output);

    (app, pty_runtime, terminal_id)
}

pub(super) fn vt100_parser(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    parser
}

/// Every symbol painted inside `area`, in reading order.
pub(super) fn painted_area(backend: &TestBackend, area: Rect) -> String {
    let buffer = backend.buffer();
    (0..area.height)
        .flat_map(|row| (0..area.width).map(move |col| (row, col)))
        .map(|(row, col)| {
            buffer
                .cell((area.x + col, area.y + row))
                .expect("cell is in bounds")
                .symbol()
                .to_string()
        })
        .collect()
}

pub(super) fn test_palette() -> Palette {
    config::Config::default().palette()
}

/// `ColorSchemeConfig` carries a private palette cache, so its fields
/// cannot be filled with functional-update syntax from here.
pub(super) fn colorscheme_with(mutate: impl FnOnce(&mut ColorSchemeConfig)) -> ColorSchemeConfig {
    let mut colorscheme = ColorSchemeConfig::default();
    mutate(&mut colorscheme);
    colorscheme
}
