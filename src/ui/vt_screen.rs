//! The `vt100::Screen` → `tui_term` adapter.
//!
//! A pure data conversion with no rendering in it: one frame's worth of cells,
//! copied out of the emulator into the shapes `tui_term` draws. Cell text is
//! kept inline rather than in a `String` because the whole screen is rebuilt
//! every frame (R6).

use ratatui::style::{Color, Modifier, Style};
use tui_term::widget::{Cell as TerminalCellWidget, Screen as TerminalScreenWidget};

/// Bytes of cell contents stored inline. `vt100` caps a cell at six `char`s
/// (`CODEPOINTS_IN_CELL`) — a base character plus combining marks — so 6 × 4
/// bytes covers every cell the parser can produce, and the heap arm below is a
/// safety net rather than a live path.
const INLINE_SYMBOL_BYTES: usize = 24;

/// A cell's text, kept out of the heap. `TerminalScreen` rebuilds every cell on
/// every frame, so a `String` here was one allocation per cell per frame.
#[derive(Debug, Clone)]
enum CellSymbol {
    Inline {
        bytes: [u8; INLINE_SYMBOL_BYTES],
        len: u8,
    },
    Spilled(Box<str>),
}

impl CellSymbol {
    const EMPTY: Self = Self::Inline {
        bytes: [0; INLINE_SYMBOL_BYTES],
        len: 0,
    };

    fn new(contents: &str) -> Self {
        let Ok(len) = u8::try_from(contents.len()) else {
            return Self::Spilled(Box::from(contents));
        };
        if usize::from(len) > INLINE_SYMBOL_BYTES {
            return Self::Spilled(Box::from(contents));
        }

        let mut bytes = [0; INLINE_SYMBOL_BYTES];
        bytes[..contents.len()].copy_from_slice(contents.as_bytes());
        Self::Inline { bytes, len }
    }

    fn as_str(&self) -> &str {
        match self {
            // Always built from a `&str`, so the slice is valid UTF-8 by
            // construction; the fallback keeps this free of `unsafe`.
            Self::Inline { bytes, len } => {
                std::str::from_utf8(&bytes[..usize::from(*len)]).unwrap_or_default()
            }
            Self::Spilled(contents) => contents,
        }
    }
}

#[derive(Debug)]
pub(super) struct TerminalScreen {
    rows: u16,
    cols: u16,
    cursor_position: (u16, u16),
    hide_cursor: bool,
    cells: Vec<TerminalCell>,
}

#[derive(Debug, Clone)]
pub(super) struct TerminalCell {
    symbol: CellSymbol,
    has_contents: bool,
    style: Style,
}

impl TerminalScreen {
    pub(super) fn from_vt100(screen: &vt100::Screen) -> Self {
        let (rows, cols) = screen.size();
        let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
        for row in 0..rows {
            for col in 0..cols {
                cells.push(
                    screen
                        .cell(row, col)
                        .map(TerminalCell::from_vt100)
                        .unwrap_or_default(),
                );
            }
        }
        let (cursor_row, cursor_col) = screen.cursor_position();
        let scrollback = u16::try_from(screen.scrollback()).unwrap_or(u16::MAX);

        Self {
            rows,
            cols,
            cursor_position: (cursor_row.saturating_add(scrollback), cursor_col),
            hide_cursor: screen.hide_cursor(),
            cells,
        }
    }

    fn cell_index(&self, row: u16, col: u16) -> Option<usize> {
        if row >= self.rows || col >= self.cols {
            return None;
        }

        Some(usize::from(row) * usize::from(self.cols) + usize::from(col))
    }
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            symbol: CellSymbol::EMPTY,
            has_contents: false,
            style: Style::reset(),
        }
    }
}

impl TerminalCell {
    fn from_vt100(cell: &vt100::Cell) -> Self {
        let mut modifier = Modifier::empty();
        if cell.bold() {
            modifier |= Modifier::BOLD;
        }
        if cell.italic() {
            modifier |= Modifier::ITALIC;
        }
        if cell.underline() {
            modifier |= Modifier::UNDERLINED;
        }
        if cell.inverse() {
            modifier |= Modifier::REVERSED;
        }

        // `vt100::Cell::contents` builds an owned `String`, and a blank cell's
        // symbol is never rendered (see `apply`), so blanks skip the call
        // entirely and the rest copy the result inline instead of cloning it
        // onto the heap a second time.
        let has_contents = cell.has_contents();
        let symbol = if has_contents {
            CellSymbol::new(&cell.contents())
        } else {
            CellSymbol::EMPTY
        };

        Self {
            symbol,
            has_contents,
            style: Style::reset()
                .fg(vt100_color_to_ratatui(cell.fgcolor()))
                .bg(vt100_color_to_ratatui(cell.bgcolor()))
                .add_modifier(modifier),
        }
    }
}

impl TerminalScreenWidget for TerminalScreen {
    type C = TerminalCell;

    fn cell(&self, row: u16, col: u16) -> Option<&Self::C> {
        self.cell_index(row, col)
            .and_then(|index| self.cells.get(index))
    }

    fn hide_cursor(&self) -> bool {
        self.hide_cursor
    }

    fn cursor_position(&self) -> (u16, u16) {
        self.cursor_position
    }
}

impl TerminalCellWidget for TerminalCell {
    fn has_contents(&self) -> bool {
        self.has_contents
    }

    fn apply(&self, cell: &mut ratatui::buffer::Cell) {
        if self.has_contents {
            cell.set_symbol(self.symbol.as_str());
        }
        cell.set_style(self.style);
    }
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;

    use super::*;

    use crate::ui::test_support::*;
    use ratatui::layout::Rect;

    #[test]
    fn vt100_attributes_map_to_ratatui_modifiers() {
        let parser = vt100_parser(
            1,
            8,
            b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[4mU\x1b[0m\x1b[7mR\x1b[0m\x1b[1;3;4;7mA\x1b[0m",
        );
        let screen = TerminalScreen::from_vt100(parser.screen());
        let cell = |col| screen.cell(0, col).expect("cell is in bounds");

        assert_eq!(cell(0).symbol.as_str(), "B");
        assert_eq!(cell(0).style.add_modifier, Modifier::BOLD);
        assert_eq!(cell(1).style.add_modifier, Modifier::ITALIC);
        assert_eq!(cell(2).style.add_modifier, Modifier::UNDERLINED);
        assert_eq!(cell(3).style.add_modifier, Modifier::REVERSED);
        assert_eq!(
            cell(4).style.add_modifier,
            Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::REVERSED
        );
        // An untouched cell carries no attributes at all.
        assert_eq!(cell(5).style.add_modifier, Modifier::empty());
    }

    #[test]
    fn vt100_color_variants_map_to_reset_indexed_and_rgb() {
        assert_eq!(vt100_color_to_ratatui(vt100::Color::Default), Color::Reset);
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Idx(9)),
            Color::Indexed(9)
        );
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Rgb(1, 2, 3)),
            Color::Rgb(1, 2, 3)
        );

        // ...and the same three shapes as the adapter sees them: default,
        // an SGR palette index, and a 24-bit foreground over an indexed
        // background.
        let parser = vt100_parser(
            1,
            8,
            b"d\x1b[31mi\x1b[0m\x1b[38;2;10;20;30m\x1b[48;5;42mr\x1b[0m",
        );
        let screen = TerminalScreen::from_vt100(parser.screen());
        let cell = |col| screen.cell(0, col).expect("cell is in bounds");

        assert_eq!(cell(0).style.fg, Some(Color::Reset));
        assert_eq!(cell(0).style.bg, Some(Color::Reset));
        assert_eq!(cell(1).style.fg, Some(Color::Indexed(1)));
        assert_eq!(cell(2).style.fg, Some(Color::Rgb(10, 20, 30)));
        assert_eq!(cell(2).style.bg, Some(Color::Indexed(42)));
    }

    #[test]
    fn wide_cell_occupies_one_symbol_and_blank_successor() {
        let parser = vt100_parser(1, 8, "你a".as_bytes());
        let screen = TerminalScreen::from_vt100(parser.screen());
        let cell = |col| screen.cell(0, col).expect("cell is in bounds");

        // The wide glyph lives in a single grid cell and carries the whole
        // character; the column it visually covers is its continuation.
        assert!(cell(0).has_contents());
        assert_eq!(cell(0).symbol.as_str(), "你");
        // vt100 marks a wide continuation by setting a flag bit in the same
        // byte that stores the length, so `has_contents` reports `true` for it
        // while its *contents* are empty. Keying the symbol off the contents —
        // not off the flag — is what stops the pair being overprinted with a
        // stray glyph, and it is why the adapter cannot treat `has_contents` as
        // "non-empty".
        assert!(cell(1).has_contents());
        assert_eq!(cell(1).symbol.as_str(), "");
        // The next character resumes at the column after the pair.
        assert!(cell(2).has_contents());
        assert_eq!(cell(2).symbol.as_str(), "a");

        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let target = &mut buffer[(0, 0)];
        target.set_symbol("stale");
        cell(1).apply(target);
        assert_eq!(target.symbol(), "");
    }

    #[test]
    fn cell_symbols_survive_combining_marks_and_the_inline_capacity_boundary() {
        // vt100 packs a base character plus combining marks into one cell, up
        // to six codepoints; all of them must reach the buffer.
        let with_marks = "e\u{0301}\u{0302}\u{0303}\u{0304}\u{0305}";
        let parser = vt100_parser(1, 4, with_marks.as_bytes());
        let screen = TerminalScreen::from_vt100(parser.screen());
        assert_eq!(
            screen
                .cell(0, 0)
                .expect("cell is in bounds")
                .symbol
                .as_str(),
            with_marks
        );

        // Six four-byte codepoints is the largest cell vt100 can build, and it
        // is exactly the inline capacity: it must stay inline and round-trip.
        let at_capacity = "\u{1f600}".repeat(6);
        assert_eq!(at_capacity.len(), INLINE_SYMBOL_BYTES);
        assert!(matches!(
            CellSymbol::new(&at_capacity),
            CellSymbol::Inline { .. }
        ));
        assert_eq!(CellSymbol::new(&at_capacity).as_str(), at_capacity);

        // One byte past spills to the heap rather than truncating.
        let past_capacity = format!("{at_capacity}a");
        assert!(matches!(
            CellSymbol::new(&past_capacity),
            CellSymbol::Spilled(_)
        ));
        assert_eq!(CellSymbol::new(&past_capacity).as_str(), past_capacity);

        assert_eq!(CellSymbol::EMPTY.as_str(), "");
        assert_eq!(TerminalCell::default().symbol.as_str(), "");
    }
}
