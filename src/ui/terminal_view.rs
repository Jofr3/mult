//! Drawing a live PTY screen into a pane: the emulator floor below which a
//! screen cannot be shown, and the text-selection highlight painted over it.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Paragraph},
    Frame,
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::app::TextSelection;

use super::main_pane::pane_style;
use super::theme::Palette;
use super::vt_screen::TerminalScreen;

pub(super) fn render_terminal_parser(
    frame: &mut Frame,
    area: Rect,
    parser: &vt100::Parser,
    focused: bool,
    palette: Palette,
    selection: Option<&TextSelection>,
) {
    if !area_fits_a_screen(area) {
        render_pane_too_small(frame, area, focused, palette);
        return;
    }
    // Without colour the block would be invisible against the default
    // foreground, so the cursor cell is reverse video instead (E10).
    let cursor_style = if palette.monochrome {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(palette.cursor).bg(palette.base)
    };
    let cursor_overlay_style = palette.emphasis(palette.nc, palette.cursor);
    let cursor = Cursor::default()
        .symbol("█")
        .style(cursor_style)
        .overlay_style(cursor_overlay_style)
        .visibility(parser.screen().scrollback() == 0);
    let terminal_screen = TerminalScreen::from_vt100(parser.screen());
    let pseudo_term = PseudoTerminal::new(&terminal_screen)
        .block(Block::default().style(pane_style(focused, palette)))
        .cursor(cursor);
    frame.render_widget(pseudo_term, area);
    if let Some(selection) = selection {
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let row_content_widths: Vec<u16> = (0..rows)
            .map(|row| {
                (0..cols)
                    .rev()
                    .find(|&col| screen.cell(row, col).is_some_and(vt100::Cell::has_contents))
                    .map_or(0, |col| col.saturating_add(1))
            })
            .collect();
        render_text_selection(frame, area, &row_content_widths, selection, palette);
    }
}

/// Whether a pane area can hold a screen at the size the emulator is actually
/// driven at.
///
/// The parser is never built below `MIN_SCREEN_ROWS`×`MIN_SCREEN_COLS`
/// (see [`crate::pty::PtyDimensions::clamped`] — anything smaller panics
/// `fnug-vt100`), so a smaller area cannot show its screen: it would render a
/// crop of a differently sized terminal, with the cursor and every mouse
/// coordinate off. Say so instead.
fn area_fits_a_screen(area: Rect) -> bool {
    crate::pty::PtyDimensions {
        rows: area.height,
        cols: area.width,
    }
    .fits_a_screen()
}

/// Fill a pane that is too small to draw with a visible marker. Anything wider
/// than a couple of columns gets words; below that the pane is filled with `!`
/// so the degradation still reads as deliberate rather than as a blank pane.
fn render_pane_too_small(frame: &mut Frame, area: Rect, focused: bool, palette: Palette) {
    let style = pane_style(focused, palette).fg(palette.gold);
    let label = if area.width >= 9 {
        "too small".to_string()
    } else {
        "!".repeat(usize::from(area.width))
    };
    let lines = (0..area.height)
        .map(|row| {
            if row == area.height / 2 {
                Line::from(label.clone())
            } else {
                Line::from("")
            }
        })
        .collect::<Vec<_>>();
    let paragraph = Paragraph::new(lines)
        .block(Block::default().style(style))
        .style(style);
    frame.render_widget(paragraph, area);
}

fn render_text_selection(
    frame: &mut Frame,
    area: Rect,
    row_content_widths: &[u16],
    selection: &TextSelection,
    palette: Palette,
) {
    let terminal_rows = u16::try_from(row_content_widths.len()).unwrap_or(u16::MAX);
    if area.is_empty() || terminal_rows == 0 {
        return;
    }

    let visible_rows = area.height.min(terminal_rows);
    let visible_last_row = i32::from(visible_rows.saturating_sub(1));
    let range = selection.normalized_range();
    if range.end.row < 0 || range.start.row > visible_last_row {
        return;
    }

    let start_row = range.start.row.max(0);
    let end_row = range.end.row.min(visible_last_row);
    let start_col = range.start.col.min(area.width.saturating_sub(1));
    let end_col = range.end.col.min(area.width.saturating_sub(1));
    let style = palette.emphasis(palette.nc, palette.foam);

    for row in start_row..=end_row {
        // Clip the highlight to the row's glyph extent so trailing blank cells
        // of short lines are not painted as selected — matching the text that
        // `contents_between` actually copies. A fully blank row highlights
        // nothing.
        let content_width = usize::try_from(row)
            .ok()
            .and_then(|index| row_content_widths.get(index))
            .copied()
            .unwrap_or(0);
        if content_width == 0 {
            continue;
        }
        let content_last_col = content_width.saturating_sub(1);
        let row_start_col = if row == range.start.row { start_col } else { 0 };
        let row_end_col = if row == range.end.row {
            end_col
        } else {
            area.width.saturating_sub(1)
        }
        .min(content_last_col);
        if row_start_col > row_end_col {
            continue;
        }
        let row = u16::try_from(row).unwrap_or(0);
        frame.buffer_mut().set_style(
            Rect::new(
                area.x.saturating_add(row_start_col),
                area.y.saturating_add(row),
                row_end_col.saturating_sub(row_start_col).saturating_add(1),
                1,
            ),
            style,
        );
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::app::App;
    use crate::app::NavItem;
    use crate::app::SelectionCell;
    use crate::config;

    use crate::layout::AppLayout;
    use crate::model::PtyKey;
    use crate::pty::PtyRuntime;
    use crate::ui::test_support::*;

    #[test]
    fn scrolled_terminal_output_hides_cursor() {
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, crate::pty::PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree");
        assert!(pty_runtime.scroll_up(terminal_id, 1).expect("scroll up"));

        let text = draw_text(&app, &pty_runtime, 50, 6);

        assert!(!text.contains('█'));
    }

    #[test]
    fn terminal_cursor_uses_white_on_blank_cell() {
        let mut app = App::default();
        let nav_items = app.nav_items();
        let (selected, terminal_id) = nav_items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);

        let frame_area = Rect::new(0, 0, 50, 6);
        let (_, output_area) = AppLayout::compute(&app, frame_area)
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");
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
        pty_runtime.process_terminal_output(terminal_id, b"x");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw_app(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        let palette = test_palette();
        let cursor_cell = terminal
            .backend()
            .buffer()
            .cell((output_area.x + 1, output_area.y))
            .expect("cursor cell is in bounds");
        assert_eq!(cursor_cell.symbol(), "█");
        assert_eq!(cursor_cell.fg, palette.cursor);
        assert_eq!(cursor_cell.bg, palette.base);
        assert_ne!(cursor_cell.fg, palette.nc);
    }

    #[test]
    fn terminal_text_selection_is_highlighted_in_main_pane() {
        let mut app = App::default();
        let nav_items = app.nav_items();
        let (selected, terminal_id) = nav_items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 0, col: 1 });

        let frame_area = Rect::new(0, 0, 50, 6);
        let (_, output_area) = AppLayout::compute(&app, frame_area)
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");
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
        pty_runtime.process_terminal_output(terminal_id, b"xy");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw_app(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        let palette = test_palette();
        let selected_cell = terminal
            .backend()
            .buffer()
            .cell((output_area.x, output_area.y))
            .expect("selected cell is in bounds");
        assert_eq!(selected_cell.symbol(), "x");
        assert_eq!(selected_cell.fg, palette.nc);
        assert_eq!(selected_cell.bg, palette.foam);
    }

    #[test]
    fn wide_char_in_selection_is_highlighted() {
        let mut app = App::default();
        let nav_items = app.nav_items();
        let (selected, terminal_id) = nav_items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        // Select 'a' (col 0) through the wide '你' (col 1, spanning cols 1-2).
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 0, col: 1 });

        let frame_area = Rect::new(0, 0, 50, 6);
        let (_, output_area) = AppLayout::compute(&app, frame_area)
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");
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
        pty_runtime.process_terminal_output(terminal_id, "a你b".as_bytes());

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw_app(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        let palette = test_palette();
        let buffer = terminal.backend().buffer();
        let narrow = buffer
            .cell((output_area.x, output_area.y))
            .expect("'a' cell is in bounds");
        assert_eq!(narrow.symbol(), "a");
        assert_eq!(narrow.bg, palette.foam);
        // The wide glyph (one grid cell occupying two screen columns) is also
        // highlighted; selecting its cell paints the whole glyph.
        let wide = buffer
            .cell((output_area.x + 1, output_area.y))
            .expect("wide cell is in bounds");
        assert_eq!(wide.bg, palette.foam);
    }

    #[test]
    fn multiline_selection_does_not_highlight_trailing_blanks_of_short_rows() {
        let mut app = App::default();
        let nav_items = app.nav_items();
        let (selected, terminal_id) = nav_items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        // Select three rows top-to-bottom; the middle row is shorter than the
        // ones bracketing it.
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 2, col: 4 });

        let frame_area = Rect::new(0, 0, 50, 12);
        let (_, output_area) = AppLayout::compute(&app, frame_area)
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");
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
        pty_runtime.process_terminal_output(terminal_id, b"xxxxx\r\nab\r\nyyyyy");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw_app(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        let palette = test_palette();
        let buffer = terminal.backend().buffer();
        let bg_at = |col: u16, row: u16| {
            buffer
                .cell((output_area.x + col, output_area.y + row))
                .expect("cell is in bounds")
                .bg
        };

        // Middle row "ab": both glyphs are highlighted...
        assert_eq!(bg_at(0, 1), palette.foam);
        assert_eq!(bg_at(1, 1), palette.foam);
        // ...but the blank cells past its content are not, even though the
        // rows above and below extend across that column.
        assert_ne!(bg_at(3, 1), palette.foam);
        assert_eq!(bg_at(3, 0), palette.foam);
        assert_eq!(bg_at(3, 2), palette.foam);
    }

    #[test]
    fn offscreen_terminal_text_selection_is_not_pinned_to_pane_edge() {
        let mut app = App::default();
        let nav_items = app.nav_items();
        let (selected, terminal_id) = nav_items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        app.begin_text_selection(terminal_id, SelectionCell { row: -1, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: -1, col: 1 });

        let frame_area = Rect::new(0, 0, 50, 6);
        let (_, output_area) = AppLayout::compute(&app, frame_area)
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");
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
        pty_runtime.process_terminal_output(terminal_id, b"xy");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw_app(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        let palette = test_palette();
        let cell = terminal
            .backend()
            .buffer()
            .cell((output_area.x, output_area.y))
            .expect("cell is in bounds");
        assert_eq!(cell.symbol(), "x");
        assert_ne!(cell.bg, palette.foam);
    }

    #[test]
    fn terminal_output_does_not_wrap_styled_blank_rows() {
        let mut app = App::default();
        let nav_items = app.nav_items();
        let (selected, terminal_id) = nav_items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);

        let frame_area = Rect::new(0, 0, 50, 6);
        let (_, output_area) = AppLayout::compute(&app, frame_area)
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");
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
        let spaces = " ".repeat(usize::from(output_area.width));
        pty_runtime.process_terminal_output(
            terminal_id,
            format!("\x1b[44m{spaces}\x1b[0m\r\nnext").as_bytes(),
        );

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw_app(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        assert_eq!(
            buffer_text(terminal.backend(), output_area.x, output_area.y + 1, 4),
            "next"
        );
    }

    #[test]
    fn a_pane_below_the_emulator_floor_is_marked_too_small() {
        // The parser is never built below 2×2 (A13), so a one-row or one-column
        // pane cannot show its screen. It must say so rather than paint a crop
        // of a differently sized terminal.
        let parser = vt100_parser(2, 4, b"WXYZ");
        let palette = test_palette();

        for area in [
            Rect::new(0, 0, 1, 4),
            Rect::new(0, 0, 4, 1),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 20, 1),
        ] {
            let backend = TestBackend::new(24, 8);
            let mut terminal = Terminal::new(backend).expect("create test terminal");
            terminal
                .draw(|frame| render_terminal_parser(frame, area, &parser, true, palette, None))
                .expect("draw pane");

            let painted = painted_area(terminal.backend(), area);
            assert!(
                !painted.contains('W'),
                "{area:?} drew the screen at the wrong size: {painted:?}"
            );
            if area.width >= 9 {
                assert!(
                    painted.contains("too small"),
                    "{area:?} did not say why it is blank: {painted:?}"
                );
            } else {
                assert!(
                    painted.contains('!'),
                    "{area:?} degraded invisibly: {painted:?}"
                );
            }
        }
    }

    #[test]
    fn a_pane_at_the_emulator_floor_still_draws_its_screen() {
        let parser = vt100_parser(2, 2, b"ab");
        let area = Rect::new(0, 0, 2, 2);
        let backend = TestBackend::new(8, 8);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| render_terminal_parser(frame, area, &parser, true, test_palette(), None))
            .expect("draw pane");

        assert!(painted_area(terminal.backend(), area).starts_with("ab"));
    }
}
