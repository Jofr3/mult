//! Display-width text helpers shared by the surfaces that have to fit a label
//! into a fixed number of columns.

use ratatui::text::Span;

pub(super) fn text_width(value: &str) -> usize {
    Span::raw(value).width()
}

/// Display width of a single character. `Span::raw` borrows, so encoding into a
/// stack buffer keeps this allocation-free — this runs per character of every
/// sidebar row, twice per row, every frame.
fn char_width(ch: char) -> usize {
    let mut buffer = [0u8; 4];
    text_width(ch.encode_utf8(&mut buffer))
}

pub(super) fn truncate_text(value: &str, max_width: usize) -> String {
    if text_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let ellipsis_width = text_width("…");
    let mut output = String::new();
    let mut width = 0;
    for ch in value.chars() {
        let ch_width = char_width(ch);
        if width + ch_width + ellipsis_width > max_width {
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output.push('…');
    output
}
