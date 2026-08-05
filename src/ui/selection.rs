//! Drawing the mouse text selection over a pane's output.
//!
//! The highlight is clipped to each row's glyph extent, so the trailing blank
//! cells of a short line are not painted as selected — matching the text that
//! `contents_between` actually copies.

use ratatui::{layout::Rect, style::Style, Frame};

use crate::app::TextSelection;

use super::theme::{readable_fg, Palette};

pub(super) fn render_text_selection(
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
    let style = Style::default()
        .fg(readable_fg(palette.nc, palette.foam))
        .bg(palette.foam);

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
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

    use crate::{
        app::{App, NavItem, SelectionCell},
        config,
        layout::AppLayout,
        model::PtyKey,
        pty::PtyRuntime,
    };

    use super::super::{draw, test_support::test_palette};

    #[test]
    fn terminal_text_selection_is_highlighted_in_main_pane() {
        let mut app = App::two_workspaces();
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
            .selected_terminal_output()
            .expect("terminal selection has output area");
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(
                terminal_id,
                crate::pty::PtyDimensions::new(output_area.height, output_area.width),
            )
            .expect("resize parser");
        pty_runtime.process_pty_output(terminal_id, b"xy");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &pty_runtime,
                    &config::Config::default(),
                )
            })
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
        let mut app = App::two_workspaces();
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
            .selected_terminal_output()
            .expect("terminal selection has output area");
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(
                terminal_id,
                crate::pty::PtyDimensions::new(output_area.height, output_area.width),
            )
            .expect("resize parser");
        pty_runtime.process_pty_output(terminal_id, "a你b".as_bytes());

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &pty_runtime,
                    &config::Config::default(),
                )
            })
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
        let mut app = App::two_workspaces();
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
            .selected_terminal_output()
            .expect("terminal selection has output area");
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(
                terminal_id,
                crate::pty::PtyDimensions::new(output_area.height, output_area.width),
            )
            .expect("resize parser");
        pty_runtime.process_pty_output(terminal_id, b"xxxxx\r\nab\r\nyyyyy");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &pty_runtime,
                    &config::Config::default(),
                )
            })
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
        let mut app = App::two_workspaces();
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
            .selected_terminal_output()
            .expect("terminal selection has output area");
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(
                terminal_id,
                crate::pty::PtyDimensions::new(output_area.height, output_area.width),
            )
            .expect("resize parser");
        pty_runtime.process_pty_output(terminal_id, b"xy");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &pty_runtime,
                    &config::Config::default(),
                )
            })
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
}
