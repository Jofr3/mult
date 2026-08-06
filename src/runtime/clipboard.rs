//! Copying a text selection out of `mult`: extracting the selected text from a
//! PTY screen, queuing it, and emitting it to the host terminal as OSC 52.
//!
//! The base64 encoder is hand-rolled rather than pulled in as a dependency —
//! OSC 52 is the only thing in the client that needs one.

use std::io::{self, Write};

use ratatui::DefaultTerminal;

use crate::{
    app::{App, TextSelection},
    config::Config,
    pty::PtyRuntime,
};

pub(super) fn copy_current_text_selection(
    app: &mut App,
    pty_runtime: &PtyRuntime,
    config: &Config,
) -> bool {
    let Some(selection) = app.text_selection else {
        return false;
    };
    copy_text_selection_to_clipboard(app, pty_runtime, config, selection)
}

/// Queue the selected text for the system clipboard, unless OSC 52 is disabled.
///
/// The payload is untrusted PTY output, and OSC 52 hands it to the *host*
/// terminal's clipboard, where `mult` no longer controls it — hence the
/// `clipboard_osc52` opt-out. The default stays on so nothing changes for
/// anyone who has not asked for the change.
pub(super) fn copy_text_selection_to_clipboard(
    app: &mut App,
    pty_runtime: &PtyRuntime,
    config: &Config,
    selection: TextSelection,
) -> bool {
    if !config.clipboard_osc52 || selection.anchor == selection.focus {
        return false;
    }
    let Some(text) = selected_text(pty_runtime, selection) else {
        return false;
    };
    if text.is_empty() {
        return false;
    }

    app.queue_clipboard_text(text);
    true
}

fn selected_text(pty_runtime: &PtyRuntime, selection: TextSelection) -> Option<String> {
    let parser = pty_runtime.parser(selection.pty)?;
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return None;
    }

    let range = selection.normalized_range();
    let visible_last_row = i32::from(rows.saturating_sub(1));
    if range.end.row < 0 || range.start.row > visible_last_row {
        return None;
    }

    let start_row = range.start.row.max(0);
    let end_row = range.end.row.min(visible_last_row);
    let start_col = if start_row == range.start.row {
        range.start.col.min(cols.saturating_sub(1))
    } else {
        0
    };
    let end_col = if end_row == range.end.row {
        range.end.col.min(cols.saturating_sub(1))
    } else {
        cols.saturating_sub(1)
    };
    let start_row = u16::try_from(start_row).unwrap_or(0);
    let end_row = u16::try_from(end_row).unwrap_or(rows.saturating_sub(1));
    let end_col_exclusive = end_col.saturating_add(1).min(cols);
    if start_row == end_row && start_col >= end_col_exclusive {
        return None;
    }

    let text = screen.contents_between(start_row, start_col, end_row, end_col_exclusive);
    (!text.is_empty()).then_some(text)
}

/// Emit any queued clipboard text as OSC 52 through the terminal's own writer.
///
/// Called right after a frame completes, so the escape shares the renderer's
/// output path (and its flush ordering) instead of racing it on a second
/// `io::stdout()` handle from inside an event handler.
pub(super) fn flush_pending_clipboard(
    terminal: &mut DefaultTerminal,
    app: &mut App,
) -> io::Result<()> {
    let Some(text) = app.take_pending_clipboard() else {
        return Ok(());
    };
    if text.is_empty() {
        return Ok(());
    }

    let encoded = base64_encode(text.as_bytes());
    let writer = terminal.backend_mut();
    write!(writer, "\x1b]52;c;{encoded}\x07")?;
    writer.flush()
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let bits = ((first as u32) << 16) | ((second as u32) << 8) | third as u32;

        output.push(TABLE[((bits >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((bits >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[((bits >> 6) & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(bits & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::SelectionCell,
        model::{self, PtyKey},
        pty::PtyDimensions,
    };

    #[test]
    fn terminal_text_selection_extracts_visible_pane_text() {
        let terminal = PtyKey::Terminal(model::TerminalId::new(77).unwrap());
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.reset_parser(terminal, PtyDimensions::new(2, 8));
        pty_runtime.process_pty_output(terminal, b"abc\r\ndef");

        let selection = TextSelection {
            pty: terminal,
            anchor: SelectionCell { row: 0, col: 1 },
            focus: SelectionCell { row: 1, col: 0 },
            dragging: false,
        };

        assert_eq!(
            selected_text(&pty_runtime, selection).as_deref(),
            Some("bc\nd")
        );
    }
    #[test]
    fn wide_char_text_selection_extracts_expected_cells() {
        let terminal = PtyKey::Terminal(model::TerminalId::new(78).unwrap());
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.reset_parser(terminal, PtyDimensions::new(1, 8));
        // 'a' at col 0; the wide '你' occupies cols 1-2 (glyph at 1, continuation
        // at 2); 'b' at col 3.
        pty_runtime.process_pty_output(terminal, "a你b".as_bytes());

        let select = |start: u16, end: u16| {
            selected_text(
                &pty_runtime,
                TextSelection {
                    pty: terminal,
                    anchor: SelectionCell { row: 0, col: start },
                    focus: SelectionCell { row: 0, col: end },
                    dragging: false,
                },
            )
        };

        assert_eq!(select(0, 3).as_deref(), Some("a你b"));
        assert_eq!(select(0, 0).as_deref(), Some("a"));
        assert_eq!(select(0, 1).as_deref(), Some("a你"));
        assert_eq!(select(1, 3).as_deref(), Some("你b"));
        assert_eq!(select(3, 3).as_deref(), Some("b"));
    }
    #[test]
    fn osc52_clipboard_writes_are_queued_and_can_be_turned_off() {
        let terminal = PtyKey::Terminal(model::TerminalId::new(79).unwrap());
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.reset_parser(terminal, PtyDimensions::new(1, 8));
        pty_runtime.process_pty_output(terminal, b"secret");
        let selection = TextSelection {
            pty: terminal,
            anchor: SelectionCell { row: 0, col: 0 },
            focus: SelectionCell { row: 0, col: 5 },
            dragging: false,
        };
        let mut app = App::two_workspaces();

        // Default: unchanged behaviour, but queued for the render loop to emit
        // rather than written straight to stdout from the event handler.
        let mut config = Config::default();
        assert!(copy_text_selection_to_clipboard(
            &mut app,
            &pty_runtime,
            &config,
            selection
        ));
        assert_eq!(app.take_pending_clipboard().as_deref(), Some("secret"));
        assert_eq!(app.take_pending_clipboard(), None);

        // Opted out: nothing leaves the process at all.
        config.clipboard_osc52 = false;
        assert!(!copy_text_selection_to_clipboard(
            &mut app,
            &pty_runtime,
            &config,
            selection
        ));
        assert_eq!(app.take_pending_clipboard(), None);
    }
    #[test]
    fn base64_encode_pads_clipboard_payloads() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }
}
