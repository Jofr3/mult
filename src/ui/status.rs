//! The status surface: transient and sticky notices drawn above the prompt.
//!
//! It only exists while it has something to say, so a quiet session gives every
//! row back to the panes (E2).

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::App;

use super::theme::Palette;

/// Draw the notice surface at the top of `area` and return what is left for the
/// prompt below it. The surface takes exactly one row per live notice, so a
/// session with nothing to say gives the whole area back.
pub(super) fn draw_notice_surface(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    palette: Palette,
) -> Rect {
    let notices = app.notices();
    let notice_height = u16::try_from(notices.len()).unwrap_or(u16::MAX);
    let [notice_area, prompt_area] =
        Layout::vertical([Constraint::Length(notice_height), Constraint::Min(0)]).areas(area);
    if !notices.is_empty() {
        let lines = notices
            .iter()
            .map(|notice| {
                Line::from(Span::styled(
                    format!("{} {}", notice_marker(notice.level()), notice.text()),
                    notice_style(notice.level(), palette),
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(palette.text).bg(palette.base)),
            notice_area,
        );
    }
    prompt_area
}

fn notice_marker(level: crate::app::NoticeLevel) -> &'static str {
    match level {
        crate::app::NoticeLevel::Info => "i",
        crate::app::NoticeLevel::Warning => "!",
        crate::app::NoticeLevel::Error => "✗",
    }
}

fn notice_style(level: crate::app::NoticeLevel, palette: Palette) -> Style {
    match level {
        crate::app::NoticeLevel::Info => palette.accent(palette.foam, false),
        crate::app::NoticeLevel::Warning => palette.accent(palette.gold, true),
        crate::app::NoticeLevel::Error => palette.accent(palette.love, true),
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    use crate::pty::PtyRuntime;
    use crate::ui::test_support::*;

    #[test]
    fn save_failure_is_rendered_persistently_without_a_prompt() {
        let mut app = App::default();
        app.record_save_failure("disk full");

        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);

        assert!(text.contains("State save failed: disk full"));
        assert!(text.contains("edit or quit to retry"));
    }
}
