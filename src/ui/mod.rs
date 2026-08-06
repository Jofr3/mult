//! Rendering: the frame layout and one module per drawn surface.
//!
//! `ui` is a `State → Frame` function and nothing else. The palette and the
//! contrast maths live in [`theme`], the `vt100` → `tui_term` conversion in
//! [`vt_screen`], and each surface draws itself in [`sidebar`], [`main_pane`],
//! [`prompt`], [`status`] and [`help`].

pub mod help;
pub mod main_pane;
pub mod prompt;
pub mod selection;
pub mod sidebar;
pub mod status;
pub mod text;
pub mod theme;
pub mod vt_screen;

#[cfg(test)]
mod test_support;

use ratatui::Frame;

use crate::{
    app::{App, FocusMode, Prompt},
    config,
    layout::AppLayout,
    pty::PtyRuntime,
};

use self::{
    help::draw_help_overlay, main_pane::draw_main, prompt::draw_prompt_area, sidebar::draw_sidebar,
    status::draw_status_line, theme::Palette,
};

pub fn draw(
    frame: &mut Frame,
    layout: &AppLayout,
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
) {
    let palette = Palette::from_config(config);

    draw_sidebar(frame, app, pty_runtime, layout.sidebar, palette);
    draw_main(frame, app, pty_runtime, layout.main, palette);
    draw_status_line(frame, app, layout.status, palette);
    draw_prompt_area(frame, app, layout.prompt, palette, config);
    if let Some(Prompt::Help(prompt)) = app.prompt() {
        draw_help_overlay(frame, layout.frame, palette, prompt.scroll);
    }
}

/// Whether `focus` is where the keyboard currently is.
///
/// One question, answered in one place: the mode enum already refuses to hold a
/// focus while a prompt is open, so this no longer re-checks `is_prompt_active`
/// on the renderer's behalf (F5).
fn focus_is_active(app: &App, focus: FocusMode) -> bool {
    app.active_focus() == Some(focus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

    use crate::app::{DeleteRequest, SelectionCell};

    use self::test_support::{draw_grid, draw_text, select_snapshot_terminal, snapshot_app};

    #[test]
    fn draw_handles_empty_workspace_list() {
        let mut app = App::two_workspaces();
        app.project.workspaces.clear();
        app.select_nav_index(0);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let pty_runtime = PtyRuntime::new_offline();

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
    }
    // The snapshots below are the only full-buffer assertions in the suite:
    // every other UI test greps one row for a substring, so a sidebar that
    // narrowed or a prompt that grew a line would pass them all (G6).

    #[test]
    fn snapshot_sidebar_and_main_pane_default_state() {
        let mut app = snapshot_app();
        select_snapshot_terminal(&mut app);

        insta::assert_snapshot!(draw_grid(&app, &PtyRuntime::new_offline(), 100, 12));
    }
    #[test]
    fn snapshot_command_palette_prompt() {
        let mut app = snapshot_app();
        select_snapshot_terminal(&mut app);
        app.begin_command_palette();
        app.push_prompt_char('n');

        insta::assert_snapshot!(draw_grid(&app, &PtyRuntime::new_offline(), 100, 14));
    }
    #[test]
    fn snapshot_terminal_with_scrollback_and_selection() {
        let mut app = snapshot_app();
        let terminal_id = select_snapshot_terminal(&mut app);

        let frame_area = Rect::new(0, 0, 100, 10);
        let output_area = AppLayout::compute(&app, frame_area).terminal_output;
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.reset_parser(
            terminal_id,
            crate::pty::PtyDimensions::new(output_area.height, output_area.width),
        );
        for line in 0..16 {
            pty_runtime.process_pty_output(terminal_id, format!("line {line}\r\n").as_bytes());
        }
        assert!(pty_runtime.scroll_up(terminal_id, 3));
        app.begin_text_selection(terminal_id, SelectionCell { row: 1, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 2, col: 3 });

        insta::assert_snapshot!(draw_grid(
            &app,
            &pty_runtime,
            frame_area.width,
            frame_area.height
        ));
    }
    #[test]
    fn snapshot_status_line_shows_an_error() {
        // The E2 surface. Compared against
        // `snapshot_sidebar_and_main_pane_default_state` (the same fixture at
        // the same size), this pins that the line costs exactly one row, that
        // it takes that row from the pane rather than overlaying it, and that
        // the way to dismiss it is on screen.
        let mut app = snapshot_app();
        select_snapshot_terminal(&mut app);
        app.set_last_error("failed to connect to mult-server: No such file or directory");

        insta::assert_snapshot!(draw_grid(&app, &PtyRuntime::new_offline(), 100, 12));
    }
    #[test]
    fn snapshot_help_overlay() {
        let mut app = snapshot_app();
        select_snapshot_terminal(&mut app);
        app.show_help();

        insta::assert_snapshot!(draw_grid(&app, &PtyRuntime::new_offline(), 100, 20));
    }
    /// The overlay on a terminal it does not fit in: it must clip and scroll,
    /// not overflow the frame or panic.
    #[test]
    fn snapshot_help_overlay_narrow() {
        let mut app = snapshot_app();
        select_snapshot_terminal(&mut app);
        app.show_help();

        insta::assert_snapshot!(draw_grid(&app, &PtyRuntime::new_offline(), 44, 10));
    }
    #[test]
    fn snapshot_confirm_delete_prompt() {
        let mut app = snapshot_app();
        select_snapshot_terminal(&mut app);
        // The fixture workspace has one chat and one terminal, so deleting the
        // terminal keeps the workspace and the prompt has no cascade line.
        assert_eq!(app.request_delete_selected(true), DeleteRequest::Confirming);

        insta::assert_snapshot!(draw_grid(&app, &PtyRuntime::new_offline(), 100, 12));
    }
    #[test]
    fn snapshot_narrow_terminal_80x24() {
        let mut app = snapshot_app();
        select_snapshot_terminal(&mut app);

        insta::assert_snapshot!(draw_grid(&app, &PtyRuntime::new_offline(), 80, 24));
    }
    /// A frame with a drag selection and a live cursor, drawn with the terminal
    /// scrolled to the bottom so both affordances are on screen at once.
    fn no_color_frame(width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut app = snapshot_app();
        let terminal_id = select_snapshot_terminal(&mut app);

        let frame_area = Rect::new(0, 0, width, height);
        let output_area = AppLayout::compute(&app, frame_area).terminal_output;
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.reset_parser(
            terminal_id,
            crate::pty::PtyDimensions::new(output_area.height, output_area.width),
        );
        // The trailing `\r` parks the child's cursor back on the `s` of "second"
        // — a cell with contents, which is what the *overlay* style paints. On a
        // blank cell the cursor is a `█` glyph instead and says nothing about
        // the overlay.
        pty_runtime.process_pty_output(terminal_id, b"selected text\r\nsecond line\r");
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 0, col: 7 });

        let config = config::Config {
            color_output: config::ColorOutput::Disabled,
            ..config::Config::default()
        };
        test_support::draw_buffer_with_config(&app, &pty_runtime, &config, width, height)
    }

    /// F15: under `NO_COLOR` the selection and the cursor overlay used to be a
    /// `readable_fg` pair on a `Color::Reset` background — a contrast ratio of
    /// 1.0, so the fallback hard-coded `Color::Rgb(255, 255, 255)`. That made a
    /// drag selection indistinguishable from unselected text *and* emitted
    /// truecolor from the one mode whose whole point is not to.
    #[test]
    fn no_color_keeps_the_selection_and_cursor_visible_without_emitting_truecolor() {
        let buffer = no_color_frame(100, 10);
        let area = *buffer.area();

        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = buffer.cell((x, y)).expect("cell is in bounds");
                assert!(
                    !matches!(cell.fg, ratatui::style::Color::Rgb(..))
                        && !matches!(cell.bg, ratatui::style::Color::Rgb(..)),
                    "cell ({x}, {y}) emits truecolor under NO_COLOR: {cell:?}"
                );
            }
        }

        let reversed = |x: u16, y: u16| {
            buffer
                .cell((x, y))
                .expect("cell is in bounds")
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        };
        let output = AppLayout::compute(&snapshot_app(), Rect::new(0, 0, 100, 10)).terminal_output;
        // The eight selected cells of "selected" are reversed; the text after
        // the selection on the same row is not.
        assert!(reversed(output.x, output.y), "the selection stands out");
        assert!(reversed(output.x + 7, output.y));
        assert!(!reversed(output.x + 9, output.y), "and stops where it ends");
        // The child's cursor is parked on the `s` of "second" on the row below.
        assert!(
            reversed(output.x, output.y + 1),
            "the cursor overlay stands out"
        );
        assert!(!reversed(output.x + 1, output.y + 1));
    }
    /// Nothing pinned the `NO_COLOR` frame before, so a fourth site drawing a
    /// background nobody can see would have gone unnoticed.
    #[test]
    fn snapshot_no_color_frame() {
        insta::assert_snapshot!(test_support::monochrome_grid(&no_color_frame(100, 10)));
    }
    #[test]
    fn extreme_terminal_sizes_render_without_panicking() {
        let app = App::two_workspaces();
        let pty_runtime = PtyRuntime::new_offline();

        // Tiny / lopsided frames must not underflow the layout or cursor math.
        // The sidebar is wider than these frames, so the main pane is starved;
        // rendering should still succeed rather than panic.
        for (width, height) in [(1, 1), (2, 3), (20, 5), (1, 40), (40, 1)] {
            let _ = draw_text(&app, &pty_runtime, width, height);
        }
    }
}
