//! The `vt100::Screen` → `tui_term` adapter.
//!
//! A pure data conversion with no ratatui layout in it: the screen is copied
//! cell by cell on every frame, so the cost of that copy is the whole reason
//! this is written out by hand rather than using `contents()` (D2).

use ratatui::style::{Color, Modifier, Style};
use tui_term::widget::{Cell as TerminalCellWidget, Screen as TerminalScreenWidget};

#[derive(Debug)]
pub(super) struct TerminalScreen {
    pub(super) rows: u16,
    pub(super) cols: u16,
    pub(super) cursor_position: (u16, u16),
    pub(super) hide_cursor: bool,
    pub(super) cells: Vec<TerminalCell>,
}

#[derive(Debug, Clone)]
pub(super) struct TerminalCell {
    pub(super) symbol: CellSymbol,
    pub(super) has_contents: bool,
    pub(super) style: Style,
}

/// Bytes a [`CellSymbol`] holds without touching the heap.
///
/// A vt100 cell holds a base glyph plus up to five combining marks, so 24 bytes
/// is the widest symbol this adapter can be handed. The screen is copied cell by
/// cell on every frame, so a `String` per cell meant a heap allocation per cell
/// per frame plus the same number of frees when the copy was dropped (D2).
pub(super) const INLINE_SYMBOL_BYTES: usize = 24;

/// A grid cell's symbol, stored inline for every symbol vt100 can produce.
#[derive(Debug, Clone)]
pub(super) enum CellSymbol {
    Inline {
        bytes: [u8; INLINE_SYMBOL_BYTES],
        len: u8,
    },
    /// A symbol too long to store inline. Unreachable through vt100, but a
    /// wrong-looking glyph is a worse failure than an allocation, so oversized
    /// input is rendered rather than truncated.
    Heap(String),
}

impl Default for CellSymbol {
    fn default() -> Self {
        Self::Inline {
            bytes: [0; INLINE_SYMBOL_BYTES],
            len: 0,
        }
    }
}

impl CellSymbol {
    pub(super) fn new(symbol: &str) -> Self {
        match u8::try_from(symbol.len()) {
            Ok(len) if usize::from(len) <= INLINE_SYMBOL_BYTES => {
                let mut bytes = [0; INLINE_SYMBOL_BYTES];
                bytes[..symbol.len()].copy_from_slice(symbol.as_bytes());
                Self::Inline { bytes, len }
            }
            _ => Self::Heap(symbol.to_string()),
        }
    }

    pub(super) fn as_str(&self) -> &str {
        match self {
            // The bytes were copied verbatim out of a `&str`, so the validation
            // always succeeds; it is how the borrow is obtained without `unsafe`.
            Self::Inline { bytes, len } => {
                std::str::from_utf8(&bytes[..usize::from(*len)]).unwrap_or_default()
            }
            Self::Heap(symbol) => symbol,
        }
    }
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

    pub(super) fn cell_index(&self, row: u16, col: u16) -> Option<usize> {
        if row >= self.rows || col >= self.cols {
            return None;
        }

        Some(usize::from(row) * usize::from(self.cols) + usize::from(col))
    }
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            symbol: CellSymbol::default(),
            has_contents: false,
            style: Style::reset(),
        }
    }
}

impl TerminalCell {
    pub(super) fn from_vt100(cell: &vt100::Cell) -> Self {
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

        // `contents()` allocates a `String` unconditionally, including for the
        // blank cells that make up most of a screen, and only the cell's own
        // `has_contents` decides whether the symbol is ever drawn — so ask for
        // it only when there is a glyph to copy.
        let has_contents = cell.has_contents();
        Self {
            symbol: if has_contents {
                CellSymbol::new(&cell.contents())
            } else {
                CellSymbol::default()
            },
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

pub(super) fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt100_attributes_map_to_ratatui_modifiers() {
        let mut parser = vt100::Parser::new(1, 8, 0);
        // One column per attribute, each reset before the next so the columns
        // are independent.
        parser.process(b"p\x1b[1mb\x1b[0;3mi\x1b[0;4mu\x1b[0;7mv\x1b[0;1;3;4;7ma");
        let screen = TerminalScreen::from_vt100(parser.screen());
        let modifiers = |col| {
            TerminalScreenWidget::cell(&screen, 0, col)
                .expect("cell is in bounds")
                .style
                .add_modifier
        };

        assert_eq!(modifiers(0), Modifier::empty());
        assert_eq!(modifiers(1), Modifier::BOLD);
        assert_eq!(modifiers(2), Modifier::ITALIC);
        assert_eq!(modifiers(3), Modifier::UNDERLINED);
        assert_eq!(modifiers(4), Modifier::REVERSED);
        assert_eq!(
            modifiers(5),
            Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::REVERSED
        );
    }
    #[test]
    fn vt100_color_variants_map_to_reset_indexed_and_rgb() {
        assert_eq!(vt100_color_to_ratatui(vt100::Color::Default), Color::Reset);
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Idx(4)),
            Color::Indexed(4)
        );
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Rgb(1, 2, 3)),
            Color::Rgb(1, 2, 3)
        );

        // ...and through the adapter, where the mapping is actually used.
        let mut parser = vt100::Parser::new(1, 8, 0);
        parser.process(b"d\x1b[31;44mi\x1b[0;38;2;10;20;30mr");
        let screen = TerminalScreen::from_vt100(parser.screen());
        let style = |col| {
            TerminalScreenWidget::cell(&screen, 0, col)
                .expect("cell is in bounds")
                .style
        };

        assert_eq!(style(0).fg, Some(Color::Reset));
        assert_eq!(style(0).bg, Some(Color::Reset));
        assert_eq!(style(1).fg, Some(Color::Indexed(1)));
        assert_eq!(style(1).bg, Some(Color::Indexed(4)));
        assert_eq!(style(2).fg, Some(Color::Rgb(10, 20, 30)));
    }
    #[test]
    fn wide_cell_occupies_one_symbol_and_blank_successor() {
        let mut parser = vt100::Parser::new(1, 8, 0);
        // A wide glyph, then a base character carrying five combining marks —
        // the widest symbol a vt100 cell can hold, and the case a fixed-size
        // inline symbol buffer has to get right.
        parser.process("你a\u{0301}\u{0302}\u{0303}\u{0304}\u{0305}b".as_bytes());
        let screen = TerminalScreen::from_vt100(parser.screen());
        let symbol = |col| {
            TerminalScreenWidget::cell(&screen, 0, col)
                .expect("cell is in bounds")
                .symbol
                .as_str()
        };

        // The wide glyph lives entirely in its first column; the column it
        // spans into carries no symbol of its own.
        assert_eq!(symbol(0), "你");
        assert_eq!(symbol(1), "");
        assert_eq!(symbol(2), "a\u{0301}\u{0302}\u{0303}\u{0304}\u{0305}");
        assert_eq!(symbol(3), "b");
        assert!(TerminalScreenWidget::cell(&screen, 0, 4)
            .is_some_and(|cell| !TerminalCellWidget::has_contents(cell)));
        // Out of bounds in either axis is absent rather than wrapping around.
        assert!(TerminalScreenWidget::cell(&screen, 0, 8).is_none());
        assert!(TerminalScreenWidget::cell(&screen, 1, 0).is_none());
    }
    #[test]
    fn cell_symbols_round_trip_inline_and_oversized() {
        assert_eq!(CellSymbol::default().as_str(), "");
        assert_eq!(CellSymbol::new("").as_str(), "");
        assert_eq!(CellSymbol::new("你").as_str(), "你");

        // Exactly the inline capacity, then one byte past it: both have to read
        // back verbatim, the second one off the heap.
        let full = "a".repeat(INLINE_SYMBOL_BYTES);
        let oversized = "a".repeat(INLINE_SYMBOL_BYTES + 1);
        assert!(matches!(CellSymbol::new(&full), CellSymbol::Inline { .. }));
        assert_eq!(CellSymbol::new(&full).as_str(), full);
        assert!(matches!(CellSymbol::new(&oversized), CellSymbol::Heap(_)));
        assert_eq!(CellSymbol::new(&oversized).as_str(), oversized);
    }
}
