//! Mouse text selection over a terminal pane, in cells.
//!
//! Rows are signed because a selection can extend into scrollback, above the
//! visible screen.

use crate::model::PtyKey;

use super::*;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionCell {
    pub row: i32,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub terminal: PtyKey,
    pub anchor: SelectionCell,
    pub focus: SelectionCell,
    pub dragging: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelectionRange {
    pub start: SelectionCell,
    pub end: SelectionCell,
}

/// A left press over a pane that reports the mouse but never asked for motion
/// tracking, held back until the gesture says which of us it belongs to.
///
/// Such a program — Claude Code, in DECSET 1000 — has no way to interpret a
/// drag, so a drag over its pane is our own selection. But the press that
/// begins one is indistinguishable from the press that begins a click, and a
/// click *is* the program's. Holding the press until the first drag or the
/// release decides is what lets the one gesture serve both, and it is why the
/// program is never handed half of one: it gets the press and the release
/// together or neither.
///
/// The modifiers travel with it so the press is replayed as it happened rather
/// than as a bare click. `shift` is among them for completeness only — Shift
/// takes the pointer back before routing ever reaches this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeldPaneClick {
    pub terminal: PtyKey,
    pub cell: SelectionCell,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl TextSelection {
    pub fn normalized_range(self) -> TextSelectionRange {
        let anchor_key = (self.anchor.row, self.anchor.col);
        let focus_key = (self.focus.row, self.focus.col);
        if anchor_key <= focus_key {
            TextSelectionRange {
                start: self.anchor,
                end: self.focus,
            }
        } else {
            TextSelectionRange {
                start: self.focus,
                end: self.anchor,
            }
        }
    }
}

impl App {
    pub fn begin_text_selection(&mut self, terminal: PtyKey, cell: SelectionCell) {
        self.text_selection = Some(TextSelection {
            terminal,
            anchor: cell,
            focus: cell,
            dragging: true,
        });
    }

    pub fn update_text_selection(&mut self, terminal: PtyKey, cell: SelectionCell) -> bool {
        let Some(selection) = &mut self.text_selection else {
            return false;
        };
        if selection.terminal != terminal {
            return false;
        }
        selection.focus = cell;
        true
    }

    pub fn end_text_selection(
        &mut self,
        terminal: PtyKey,
        cell: SelectionCell,
    ) -> Option<TextSelection> {
        if !self.update_text_selection(terminal, cell) {
            return None;
        }
        if let Some(selection) = &mut self.text_selection {
            selection.dragging = false;
            Some(*selection)
        } else {
            None
        }
    }

    pub fn clear_text_selection(&mut self) {
        self.text_selection = None;
    }

    pub fn shift_text_selection_rows(&mut self, terminal: PtyKey, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let Some(selection) = &mut self.text_selection else {
            return false;
        };
        if selection.terminal != terminal {
            return false;
        }

        let anchor_row = selection.anchor.row.saturating_add(delta);
        let focus_row = selection.focus.row.saturating_add(delta);
        if selection.anchor.row == anchor_row && selection.focus.row == focus_row {
            return false;
        }
        selection.anchor.row = anchor_row;
        selection.focus.row = focus_row;
        true
    }

    pub fn text_selection_for(&self, terminal: PtyKey) -> Option<&TextSelection> {
        self.text_selection
            .as_ref()
            .filter(|selection| selection.terminal == terminal)
    }

    pub fn hold_pane_click(&mut self, held: HeldPaneClick) {
        self.held_pane_click = Some(held);
    }

    pub fn held_pane_click(&self) -> Option<HeldPaneClick> {
        self.held_pane_click
    }

    /// Take the held press, but only if it belongs to `terminal`: a gesture
    /// that has crossed into another pane no longer resolves this one.
    pub fn take_held_pane_click(&mut self, terminal: PtyKey) -> Option<HeldPaneClick> {
        let held = self
            .held_pane_click
            .filter(|held| held.terminal == terminal)?;
        self.held_pane_click = None;
        Some(held)
    }

    pub fn take_any_held_pane_click(&mut self) -> Option<HeldPaneClick> {
        self.held_pane_click.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_selection_rows_shift_with_viewport_scroll() {
        let mut app = App::default();
        let terminal = PtyKey::Terminal(TerminalId(9));
        app.begin_text_selection(terminal, SelectionCell { row: 1, col: 0 });
        app.update_text_selection(terminal, SelectionCell { row: 1, col: 2 });

        assert!(app.shift_text_selection_rows(terminal, 3));
        let selection = app.text_selection_for(terminal).expect("selection remains");
        assert_eq!(selection.anchor.row, 4);
        assert_eq!(selection.focus.row, 4);

        assert!(app.shift_text_selection_rows(terminal, -5));
        let selection = app.text_selection_for(terminal).expect("selection remains");
        assert_eq!(selection.anchor.row, -1);
        assert_eq!(selection.focus.row, -1);
    }
}
