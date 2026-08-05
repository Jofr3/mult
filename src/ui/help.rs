//! The keybinding overlay (E4).
//!
//! Every row comes from `app::keybinding_help_rows`, i.e. from the same table
//! the command palette is generated from, so there is no second list here to
//! fall out of date.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::app::{keybinding_help_rows, HelpRow};

use super::{
    text::{text_width, truncate_text},
    theme::Palette,
};

/// The keybinding overlay (E4).
///
/// Every row comes from `app::keybinding_help_rows`, i.e. from the same table
/// the command palette is generated from, so there is no second list here to
/// fall out of date. The overlay is centred and clipped to whatever the frame
/// has: on a terminal too short for the whole list it scrolls, and on one too
/// narrow the columns collapse rather than overflow.
pub(super) fn draw_help_overlay(
    frame: &mut Frame,
    frame_area: Rect,
    palette: Palette,
    scroll: usize,
) {
    let area = centered_overlay(frame_area, 78, 26);
    if area.width == 0 || area.height == 0 {
        return;
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Keybindings ")
        .title_bottom(" esc closes • ↑/↓ scrolls ")
        .style(Style::default().fg(palette.text).bg(palette.base));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rows = keybinding_help_rows();
    let visible = usize::from(inner.height);
    let scroll = scroll.min(rows.len().saturating_sub(visible));
    // Widest keys column, so labels line up; capped so a narrow overlay still
    // leaves room for the label.
    let keys_width = rows
        .iter()
        .filter_map(|row| match row {
            HelpRow::Binding(binding) => Some(text_width(help_row_keys(binding.keys))),
            HelpRow::Heading(_) => None,
        })
        .max()
        .unwrap_or(0)
        .min(usize::from(inner.width).saturating_sub(2) / 2);

    let lines = rows
        .into_iter()
        .skip(scroll)
        .take(visible)
        .map(|row| help_row_line(row, keys_width, usize::from(inner.width), palette))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub(super) fn help_row_line(
    row: HelpRow,
    keys_width: usize,
    line_width: usize,
    palette: Palette,
) -> Line<'static> {
    match row {
        HelpRow::Heading(title) => Line::from(Span::styled(
            truncate_text(title, line_width),
            Style::default()
                .fg(palette.foam)
                .add_modifier(Modifier::BOLD),
        )),
        HelpRow::Binding(binding) => {
            let keys = truncate_text(help_row_keys(binding.keys), keys_width);
            let padding = " ".repeat(keys_width.saturating_sub(text_width(&keys)));
            let described = format!("{} — {}", binding.label, binding.help);
            let described = truncate_text(
                &described,
                line_width.saturating_sub(keys_width).saturating_sub(2),
            );

            let keys_style = if binding.keys.is_empty() {
                Style::default().fg(palette.muted)
            } else {
                Style::default().fg(palette.gold)
            };

            Line::from(vec![
                Span::styled(keys, keys_style),
                Span::raw(format!("{padding}  ")),
                Span::styled(described, Style::default().fg(palette.text)),
            ])
        }
    }
}

/// What the keys column shows. A command with no key of its own says where it
/// does live, rather than leaving a blank that reads as "no way to run this".
pub(super) fn help_row_keys(keys: &'static str) -> &'static str {
    if keys.is_empty() {
        "(palette)"
    } else {
        keys
    }
}

/// A centred rect at most `width`×`height`, always inside `area`.
pub(super) fn centered_overlay(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::App, pty::PtyRuntime};

    use super::super::test_support::draw_text;

    /// Replaces `keybinding_help_line_is_not_rendered` (E4). The old test
    /// asserted the bindings appear *nowhere*; the intent worth keeping is that
    /// they never cost a permanent footer row. They now live in an overlay that
    /// is only drawn when asked for.
    #[test]
    fn keybindings_cost_no_permanent_row_and_appear_when_asked_for() {
        let mut app = App::two_workspaces();
        let pty_runtime = PtyRuntime::new_offline();

        let text = draw_text(&app, &pty_runtime, 180, 30);
        assert!(!text.contains("Ctrl+p"), "{text}");
        assert!(!text.contains("Ctrl-j/k navigate"));
        assert!(!text.contains("mouse wheel scroll"));

        app.show_help();
        let text = draw_text(&app, &pty_runtime, 180, 30);
        assert!(text.contains("Keybindings"), "{text}");
        // The palette and quit are the two bindings that were documented only
        // in the README, including the palette itself — the way you would have
        // discovered anything else.
        assert!(text.contains("Ctrl+p"), "{text}");
        assert!(text.contains("Ctrl+Esc"), "{text}");
    }
    #[test]
    fn the_help_overlay_renders_every_row_of_the_shared_binding_table() {
        let mut app = App::two_workspaces();
        app.show_help();
        let pty_runtime = PtyRuntime::new_offline();

        // The list is longer than the overlay, so read it the way a user would.
        let mut text = String::new();
        for _ in 0..keybinding_help_rows().len() {
            text.push_str(&draw_text(&app, &pty_runtime, 200, 40));
            app.scroll_help(1);
        }

        for row in keybinding_help_rows() {
            let HelpRow::Binding(binding) = row else {
                continue;
            };
            assert!(text.contains(binding.label), "{} missing", binding.label);
        }
    }
    #[test]
    fn the_help_overlay_scrolls_instead_of_hiding_its_tail_on_a_short_terminal() {
        let mut app = App::two_workspaces();
        app.show_help();
        let pty_runtime = PtyRuntime::new_offline();

        let top = draw_text(&app, &pty_runtime, 80, 10);
        assert!(top.contains("Focus sidebar"), "{top}");
        assert!(!top.contains("Copy selection"), "{top}");

        app.scroll_help(isize::MAX);
        let bottom = draw_text(&app, &pty_runtime, 80, 10);
        assert!(bottom.contains("Copy selection"), "{bottom}");
    }
    #[test]
    fn a_terminal_too_small_for_the_overlay_still_draws() {
        let mut app = App::two_workspaces();
        app.show_help();

        for (width, height) in [(1, 1), (2, 3), (12, 4), (40, 2)] {
            let _ = draw_text(&app, &PtyRuntime::new_offline(), width, height);
        }
    }
}
