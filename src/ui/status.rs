//! The global status line: one dismissible message, or nothing at all.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::{App, StatusLevel};

use super::{
    text::{text_width, truncate_text},
    theme::Palette,
};

/// The global status line: one dismissible message, or nothing at all.
///
/// Everything that has no pane to be written into lands here — the daemon
/// connection, a save that failed, the startup config warnings, the state-backup
/// notice. The hint is always drawn, and always fits: the message is what gets
/// truncated, so the way out of the message never scrolls off the end of it.
pub(super) fn draw_status_line(frame: &mut Frame, app: &App, area: Rect, palette: Palette) {
    let Some(notice) = app.current_status_notice() else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }

    let queued = app.queued_status_notice_count();
    let hint = if queued > 0 {
        format!("  (+{queued} more · ctrl-g)")
    } else {
        "  (ctrl-g dismisses)".to_string()
    };
    let level_style = Style::default().fg(match notice.level {
        StatusLevel::Info => palette.foam,
        StatusLevel::Warning => palette.gold,
        StatusLevel::Error => palette.love,
    });
    let marker = notice.level.marker();
    let message_width = usize::from(area.width)
        .saturating_sub(text_width(marker))
        .saturating_sub(text_width(&hint));

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(marker, level_style),
            Span::styled(truncate_text(&notice.message, message_width), level_style),
            Span::styled(hint, Style::default().fg(palette.muted)),
        ]))
        .style(Style::default().fg(palette.text).bg(palette.base)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use crate::layout::AppLayout;
    use ratatui::{backend::TestBackend, Terminal};

    use crate::{config, pty::PtyRuntime};

    use super::super::{
        draw,
        test_support::{buffer_text, snapshot_app},
    };

    #[test]
    fn the_status_line_truncates_the_message_and_never_the_way_out_of_it() {
        let mut app = snapshot_app();
        app.set_last_error("a".repeat(500));
        app.set_last_error("and one more");

        let backend = TestBackend::new(60, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &PtyRuntime::new_offline(),
                    &config::Config::default(),
                )
            })
            .expect("draw app");
        // No prompt is open, so the status line is the last row.
        let status_row = buffer_text(terminal.backend(), 0, 5, 60);

        assert!(status_row.starts_with("x "), "{status_row}");
        assert!(status_row.contains('…'), "{status_row}");
        assert!(status_row.contains("(+1 more · ctrl-g)"), "{status_row}");
    }
}
