//! Display-width measurement and truncation, shared by every surface that has
//! to fit text into a fixed number of columns — plus the grapheme clustering
//! the prompt cursor splits on (F3), which is defined in terms of that same
//! width and so belongs next to it.

use ratatui::text::Span;

pub(crate) fn text_width(value: &str) -> usize {
    Span::raw(value).width()
}

/// Display width of a single character, without allocating.
///
/// `text_width` needs a `&str`; encoding into a stack buffer gives it one for
/// free, where `ch.to_string()` cost a heap allocation per character — twice per
/// sidebar row, on every frame (D8). Keeping the same `text_width` underneath
/// keeps the measurement identical to the one used for whole strings.
pub(crate) fn char_width(ch: char) -> usize {
    let mut buffer = [0; 4];
    text_width(ch.encode_utf8(&mut buffer))
}

/// The zero-width joiner. It occupies no column of its own *and* pulls the
/// scalar after it into the same cluster, which is the one case display width
/// alone does not answer (`👩\u{200d}💻`).
const ZERO_WIDTH_JOINER: char = '\u{200d}';

/// Whether `ch` can begin a grapheme cluster of its own.
///
/// The test is display width, not a Unicode category table: a scalar that
/// occupies no column — a combining mark, a variation selector, a joiner —
/// can never be given a cell of its own, so it has to belong to the cluster
/// before it. That is exactly the property the renderer needs, and it needs no
/// character tables and so no new dependency.
fn starts_cluster(ch: char) -> bool {
    char_width(ch) > 0
}

/// Byte offset just past the grapheme cluster that begins at `start`.
///
/// `start` must be a char boundary; a `start` at or past the end of `text`
/// returns itself, so callers can loop on this without a length check.
pub(crate) fn cluster_end(text: &str, start: usize) -> usize {
    let Some(rest) = text.get(start..) else {
        return text.len();
    };
    let mut chars = rest.char_indices();
    let Some((_, base)) = chars.next() else {
        return start;
    };
    let mut end = base.len_utf8();
    // A cluster that somehow begins on a joiner still swallows what follows it,
    // so a degenerate string cannot make this return a half cluster.
    let mut join_next = base == ZERO_WIDTH_JOINER;
    for (index, ch) in chars {
        if !join_next && starts_cluster(ch) {
            break;
        }
        join_next = ch == ZERO_WIDTH_JOINER;
        end = index + ch.len_utf8();
    }
    start + end
}

/// Byte range of the grapheme cluster containing `offset`, or an empty range at
/// the end of `text`. An `offset` that lands *inside* a cluster snaps back to
/// its start, so a split here can never orphan a zero-width scalar.
pub(crate) fn cluster_range(text: &str, offset: usize) -> (usize, usize) {
    if offset >= text.len() {
        return (text.len(), text.len());
    }
    let mut start = 0;
    loop {
        let end = cluster_end(text, start);
        if end > offset || end == start {
            return (start, end);
        }
        start = end;
    }
}

/// Byte offset where the grapheme cluster before `offset` begins, or `0`.
pub(crate) fn previous_cluster_start(text: &str, offset: usize) -> usize {
    let mut start = 0;
    loop {
        let end = cluster_end(text, start);
        if end >= offset || end == start {
            return start;
        }
        start = end;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// F3: clustering is defined by display width, so the cases that matter to
    /// the prompt cursor — NFD marks, stacked marks, joiner sequences — come
    /// out whole without a Unicode table in the crate.
    #[test]
    fn a_grapheme_cluster_holds_its_zero_width_scalars() {
        // NFD `é`: the mark has no column of its own, so it joins the `e`.
        assert_eq!(cluster_range("jose\u{301}/x", 3), (3, 6));
        assert_eq!(
            cluster_range("jose\u{301}/x", 5),
            (3, 6),
            "snaps back into it"
        );
        assert_eq!(cluster_range("jose\u{301}/x", 6), (6, 7));
        // Stacked marks all join the same base.
        assert_eq!(cluster_range("a\u{300}\u{301}b", 0), (0, 5));
        // A joiner pulls in the glyph after it, which has a width of its own.
        assert_eq!(cluster_range("👩\u{200d}💻!", 0), (0, 11));
        assert_eq!(cluster_range("👩\u{200d}💻!", 11), (11, 12));
        // A wide character is one cluster, two columns.
        assert_eq!(cluster_range("漢z", 0), (0, 3));
        assert_eq!(text_width("漢"), 2);
        // Past the end is an empty range at the end.
        assert_eq!(cluster_range("ab", 2), (2, 2));
        assert_eq!(cluster_range("", 0), (0, 0));
    }

    #[test]
    fn the_previous_cluster_start_steps_over_whole_clusters() {
        let text = "jose\u{301}/x";
        assert_eq!(previous_cluster_start(text, 7), 6);
        assert_eq!(previous_cluster_start(text, 6), 3);
        assert_eq!(previous_cluster_start(text, 3), 2);
        assert_eq!(previous_cluster_start(text, 0), 0);
        assert_eq!(previous_cluster_start("👩\u{200d}💻!", 12), 11);
        assert_eq!(previous_cluster_start("👩\u{200d}💻!", 11), 0);
    }
}
