//! The mouse text selection over a pane's output.
//!
//! Rows are signed because a selection can sit in scrollback that is currently
//! above the viewport; scrolling shifts it rather than clamping it to the pane
//! edge.

use super::App;
use crate::model::PtyKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionCell {
    pub row: i32,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub pty: PtyKey,
    pub anchor: SelectionCell,
    pub focus: SelectionCell,
    pub dragging: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelectionRange {
    pub start: SelectionCell,
    pub end: SelectionCell,
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
    pub fn begin_text_selection(&mut self, pty: PtyKey, cell: SelectionCell) {
        self.text_selection = Some(TextSelection {
            pty,
            anchor: cell,
            focus: cell,
            dragging: true,
        });
    }

    pub fn update_text_selection(&mut self, pty: PtyKey, cell: SelectionCell) -> bool {
        let Some(selection) = &mut self.text_selection else {
            return false;
        };
        if selection.pty != pty {
            return false;
        }
        selection.focus = cell;
        true
    }

    pub fn end_text_selection(
        &mut self,
        pty: PtyKey,
        cell: SelectionCell,
    ) -> Option<TextSelection> {
        if !self.update_text_selection(pty, cell) {
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

    pub fn shift_text_selection_rows(&mut self, pty: PtyKey, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let Some(selection) = &mut self.text_selection else {
            return false;
        };
        if selection.pty != pty {
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

    pub fn text_selection_for(&self, pty: PtyKey) -> Option<&TextSelection> {
        self.text_selection
            .as_ref()
            .filter(|selection| selection.pty == pty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TerminalId;

    #[test]
    fn text_selection_rows_shift_with_viewport_scroll() {
        let mut app = App::two_workspaces();
        let terminal = PtyKey::Terminal(TerminalId::new(9).unwrap());
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
