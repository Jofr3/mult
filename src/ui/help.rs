//! The keybinding overlay.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};

use super::text::{text_width, truncate_text};
use super::theme::Palette;

/// The keybinding overlay (E4).
///
/// Generated from [`crate::app::BINDINGS`], the same table the command palette
/// filters, so a binding cannot be added to one and forgotten in the other.
/// It is drawn over the frame rather than in the layout, so it costs nothing
/// while it is down, and it degrades by truncation on a small terminal instead
/// of overflowing: rows past the bottom are dropped and a footer says so.
pub(super) fn draw_help_overlay(frame: &mut Frame, frame_area: Rect, palette: Palette) {
    if frame_area.is_empty() {
        return;
    }

    // The overlay is a bordered panel as wide as its widest row and no wider
    // than the frame. A fixed 64-column cap clipped the longest labels mid-word
    // — "Move through results (palette, projec" — with nothing on screen to say
    // they had been cut, and without a border it ran straight into whatever
    // pane it covered ("▣ websKeybindings"). `CHROME` is the two columns and two
    // rows the border costs.
    const KEY_GAP: usize = 2;
    const CHROME: u16 = 2;
    let key_width = crate::app::BINDINGS
        .iter()
        .filter_map(|binding| binding.keys)
        .map(text_width)
        .max()
        .unwrap_or(0);
    let label_width = crate::app::BINDINGS
        .iter()
        .filter(|binding| binding.keys.is_some())
        .map(|binding| text_width(binding.label))
        .max()
        .unwrap_or(0);
    let footer = "esc / ? / F1 closes • ctrl-p opens the command palette";
    let natural_width = (key_width + KEY_GAP + label_width).max(text_width(footer));
    let width = u16::try_from(natural_width)
        .unwrap_or(u16::MAX)
        .saturating_add(CHROME)
        .clamp(1, frame_area.width);
    // What is left for a label once the border, the key column and its gap are
    // paid for. Zero on a terminal narrower than the key column itself, where
    // the rows are already degenerate.
    let label_budget =
        usize::from(width.saturating_sub(CHROME)).saturating_sub(key_width + KEY_GAP);

    let mut lines = vec![Line::from(Span::styled(
        "Keybindings",
        Style::default()
            .fg(palette.foam)
            .add_modifier(Modifier::BOLD),
    ))];
    for scope in [
        crate::app::BindingScope::Global,
        crate::app::BindingScope::Prompt,
        crate::app::BindingScope::Mouse,
    ] {
        let mut heading_written = false;
        for binding in crate::app::BINDINGS
            .iter()
            .filter(|binding| binding.scope == scope)
        {
            let Some(keys) = binding.keys else {
                // Palette-only commands have no key to list; the palette itself
                // is where they are discovered, and `Ctrl+p` is listed above.
                continue;
            };
            if !heading_written {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    scope.title(),
                    palette.accent(palette.iris, true),
                )));
                heading_written = true;
            }
            let padding = " ".repeat(key_width.saturating_sub(text_width(keys)));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{keys}{padding}"),
                    palette.accent(palette.gold, false),
                ),
                Span::raw(" ".repeat(KEY_GAP)),
                // Truncated with an ellipsis rather than clipped by the
                // terminal, so a cut label says that it was cut.
                Span::raw(truncate_text(binding.label, label_budget)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        footer,
        palette.accent(palette.muted, false),
    )));

    // Centre the overlay, but never let it exceed the frame: on a terminal too
    // small for the whole list the visible part is still correct.
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(CHROME)
        .clamp(1, frame_area.height);
    let content_height = usize::from(height.saturating_sub(CHROME));
    if content_height < lines.len() {
        // Spend the last visible row saying the list is cut off rather than
        // ending mid-table with no explanation.
        lines.truncate(content_height.saturating_sub(1));
        lines.push(Line::from(Span::styled(
            "… resize for the rest",
            palette.accent(palette.muted, false),
        )));
    }
    let area = Rect {
        x: frame_area.x + (frame_area.width - width) / 2,
        y: frame_area.y + (frame_area.height - height) / 2,
        width,
        height,
    };

    let style = Style::default().fg(palette.text).bg(palette.base);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_style(Style::default().fg(palette.muted).bg(palette.base))
                    .style(style),
            )
            .style(style),
        area,
    );
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::app::App;

    use crate::pty::PtyRuntime;
    use crate::ui::test_support::*;

    #[test]
    fn keybinding_help_line_is_not_rendered() {
        let app = App::default();
        let pty_runtime = PtyRuntime::new_offline();

        let text = draw_text(&app, &pty_runtime, 180, 30);

        assert!(!text.contains("Ctrl-j/k navigate"));
        assert!(!text.contains("mouse wheel scroll"));
    }

    #[test]
    fn the_help_overlay_survives_a_terminal_too_small_to_hold_it() {
        let mut app = App::default();
        app.show_help();

        // Down to a single cell: rendering must not panic, and anything with
        // room for words must say the list is cut off rather than end
        // mid-table.
        for (width, height) in [(1, 1), (20, 4), (40, 8), (100, 12)] {
            let text = draw_text(&app, &PtyRuntime::new_offline(), width, height);
            if width >= 40 {
                assert!(
                    text.contains("resize for the rest"),
                    "{width}x{height} truncated the overlay silently"
                );
            }
        }

        // With room for everything, no truncation notice.
        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 40);
        assert!(text.contains("Keybindings"));
        assert!(text.contains("Ctrl+p"));
        assert!(!text.contains("resize for the rest"));
    }

    /// E4: the overlay used to be capped at 64 columns whatever the terminal
    /// was, so its longest labels were clipped mid-word by the renderer — "Move
    /// through results (palette, projec" — with nothing to mark the cut. It is
    /// now as wide as its widest row, and a label it genuinely cannot fit ends
    /// in an ellipsis.
    #[test]
    fn the_help_overlay_fits_its_labels_and_marks_the_ones_it_cannot() {
        let mut app = App::default();
        app.show_help();
        let longest = crate::app::BINDINGS
            .iter()
            .filter(|binding| binding.keys.is_some())
            .map(|binding| binding.label)
            .max_by_key(|label| text_width(label))
            .expect("the table has bindings with keys");

        let roomy = draw_text(&app, &PtyRuntime::new_offline(), 120, 40);
        assert!(
            roomy.contains(longest),
            "the widest label must be shown whole: {longest:?}"
        );
        assert!(
            !roomy.contains('…'),
            "nothing was cut, so nothing is marked"
        );

        // Too narrow for the longest label, but wide enough to say so.
        let narrow = draw_text(&app, &PtyRuntime::new_offline(), 60, 40);
        assert!(!narrow.contains(longest));
        assert!(
            narrow.contains('…'),
            "a clipped label must say it was clipped"
        );
    }
}
