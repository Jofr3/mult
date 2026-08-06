//! Ratatui rendering for the `mult` client.
//!
//! The module is split by surface: [`theme`] owns colour, [`vt_screen`] the
//! `vt100` → `tui_term` adapter, and one module per drawn surface. This root
//! keeps only the frame-level composition — which surface goes where, and in
//! what order. Geometry is [`crate::layout::AppLayout`]'s: the loop resolves
//! it once per iteration and hands it in, so `ui` only consumes rects (F6).

mod help;
mod main_pane;
mod prompt;
mod sidebar;
mod status;
mod terminal_view;
mod text;
pub mod theme;
mod vt_screen;

#[cfg(test)]
mod test_support;

pub use self::theme::{ColorParseIssue, Palette};

use ratatui::Frame;

use crate::{app::App, config, layout::AppLayout, pty::PtyRuntime};

use self::help::draw_help_overlay;
use self::main_pane::draw_main;
use self::prompt::draw_prompt_area;
use self::sidebar::draw_sidebar;
use self::theme::no_color_is_set;

pub fn draw(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
    layout: AppLayout,
) {
    let palette = if no_color_is_set() {
        Palette::monochrome()
    } else {
        config.palette()
    };
    draw_with_palette(frame, app, pty_runtime, config, palette, layout);
}

/// [`draw`] with the palette decided by the caller. `NO_COLOR` is a process
/// global, so this is the seam the render tests use instead of mutating it.
pub(crate) fn draw_with_palette(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
    palette: Palette,
    layout: AppLayout,
) {
    // `frame.area()` is authoritative for the frame being painted: `ratatui`
    // resizes the buffer inside `Terminal::draw`, so the first tick after a
    // host-terminal resize arrives with a layout the loop computed for the
    // previous size. Painting that one would place every surface at the old
    // geometry, and because the draw clears `needs_redraw` an idle session
    // would sit on it. The loop's layout is still the one its resize and mouse
    // handlers used this iteration, exactly as before.
    let layout = if layout.area == frame.area() {
        layout
    } else {
        AppLayout::compute(app, frame.area())
    };

    draw_sidebar(frame, app, pty_runtime, layout.sidebar, palette);
    draw_main(frame, app, pty_runtime, layout.main, palette);
    draw_prompt_area(frame, app, layout.prompt, palette, config);
    // Last, and over everything: the overlay is modal, so it must not be
    // painted under a pane, and it occupies no layout space when it is down.
    if app.is_help_visible() {
        draw_help_overlay(frame, layout.area, palette);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::app::SelectionCell;
    use crate::model::AgentKind;
    use crate::model::ChatId;
    use crate::model::ChatStatus;
    use crate::ui::test_support::*;
    use ratatui::layout::Rect;

    #[test]
    fn draw_handles_empty_workspace_list() {
        let mut app = App::default();
        app.project.workspaces.clear();
        app.select_nav_index(0);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let pty_runtime = PtyRuntime::new_offline();

        terminal
            .draw(|frame| draw_app(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");
    }

    // Snapshot scenarios deliberately avoid the chat pane's "not started" hint,
    // which prints `config::config_path()` and is therefore `$HOME`-dependent,
    // and never populate git branches, which are supplied by the caller rather
    // than probed here. Nothing else in `draw` reads the environment or a clock.

    #[test]
    fn snapshot_default_frame() {
        let app = App::default();

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            100,
            30,
        )));
    }

    #[test]
    fn snapshot_narrow_frame_80x24() {
        let app = App::default();

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            80,
            24,
        )));
    }

    #[test]
    fn snapshot_command_palette_prompt() {
        let mut app = App::default();
        app.begin_command_palette();
        app.push_prompt_char('t');

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            100,
            30,
        )));
    }

    #[test]
    fn snapshot_terminal_with_scrollback_and_selection() {
        let frame_area = Rect::new(0, 0, 60, 12);
        let (mut app, mut pty_runtime, terminal_id) = terminal_app_with_output(
            frame_area,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\neight\r\nnine\r\nten\r\neleven\r\ntwelve\r\nthirteen",
        );
        assert!(pty_runtime.scroll_up(terminal_id, 2).expect("scroll up"));
        app.begin_text_selection(terminal_id, SelectionCell { row: 1, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 2, col: 3 });

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &pty_runtime,
            &config::Config::default(),
            frame_area.width,
            frame_area.height,
        )));
    }

    #[test]
    fn snapshot_help_overlay() {
        // E4: the overlay is generated from `app::BINDINGS`, so this snapshot
        // is also the check that adding a binding without a label, or a
        // palette command without a key, changes what the user is shown.
        let mut app = App::default();
        app.show_help();

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            100,
            40,
        )));
    }

    #[test]
    fn snapshot_status_surface_with_an_error() {
        // E2: a daemon that will not connect, a save that failed and a config
        // warning all land in one dismissible surface that exists only while
        // it has something to say.
        let mut app = App::default();
        app.push_notice(
            crate::app::NoticeLevel::Error,
            crate::app::NoticeSource::Report,
            "failed to connect to mult-server: No such file or directory (os error 2)",
        );
        app.push_notice(
            crate::app::NoticeLevel::Warning,
            crate::app::NoticeSource::Report,
            "config.json: colorscheme.gold is not a #rrggbb color",
        );
        app.record_save_failure("Read-only file system (os error 30)");

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            100,
            30,
        )));
    }

    #[test]
    fn snapshot_no_color_frame() {
        // E10: every colour is `Color::Reset`, and the selected sidebar row is
        // reverse video rather than a background colour, so the frame still
        // reads. The style legend is the assertion here.
        let mut app = App::default();
        app.project.workspaces[0]
            .chats
            .push(crate::model::ChatSession {
                id: ChatId(4242),
                name: "agent".to_string(),
                status: ChatStatus::Waiting,
                agent: AgentKind::Pi,
                messages: Vec::new(),
            });

        insta::assert_snapshot!(buffer_snapshot(&render_buffer_with_palette(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            Palette::monochrome(),
            100,
            30,
        )));
    }

    #[test]
    fn extreme_terminal_sizes_render_without_panicking() {
        let app = App::default();
        let pty_runtime = PtyRuntime::new_offline();

        // Tiny / lopsided frames must not underflow the layout or cursor math.
        // The sidebar is wider than these frames, so the main pane is starved;
        // rendering should still succeed rather than panic.
        for (width, height) in [(1, 1), (2, 3), (20, 5), (1, 40), (40, 1)] {
            let _ = draw_text(&app, &pty_runtime, width, height);
        }
    }
}
