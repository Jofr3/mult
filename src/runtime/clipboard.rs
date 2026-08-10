//! Copying a text selection out to the host terminal over OSC 52, including the
//! hand-rolled base64 the sequence needs and the tmux passthrough wrapper.

use std::io::Write;

use ratatui::DefaultTerminal;

use crate::{
    app::{App, TextSelection},
    config::Config,
    pty::PtyRuntime,
};

pub(super) fn copy_current_text_selection(
    app: &App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
) -> bool {
    let Some(selection) = app.text_selection else {
        return false;
    };
    copy_text_selection_to_clipboard(pty_runtime, config, selection)
}

pub(super) fn copy_text_selection_to_clipboard(
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    selection: TextSelection,
) -> bool {
    if selection.anchor == selection.focus {
        return false;
    }
    let Some(text) = selected_text(pty_runtime, selection) else {
        return false;
    };
    copy_text_to_clipboard(pty_runtime, config, &text)
}

/// The selected text, including whatever part of it has scrolled out of the
/// view: the rows are handed to the emulator layer as they are, and it walks
/// the scrollback to reach the ones that are no longer on screen.
fn selected_text(pty_runtime: &mut PtyRuntime, selection: TextSelection) -> Option<String> {
    let range = selection.normalized_range();
    pty_runtime.contents_between_rows(
        selection.terminal,
        range.start.row,
        range.start.col,
        range.end.row,
        range.end.col.saturating_add(1),
    )
}

/// Queue an OSC 52 clipboard write for the host terminal.
///
/// Two changes from writing straight to `io::stdout()` here: the user can turn
/// it off (`clipboard_osc52`), and the sequence leaves through the frame's own
/// output after the next draw rather than from a handle grabbed inside a mouse
/// handler.
fn copy_text_to_clipboard(pty_runtime: &mut PtyRuntime, config: &Config, text: &str) -> bool {
    if text.is_empty() || !config.clipboard_osc52 {
        return false;
    }
    let sequence = osc52_clipboard_sequence(&base64_encode(text.as_bytes()), inside_tmux());
    pty_runtime.queue_host_terminal_write(&sequence);
    true
}

/// OSC 52 "set clipboard", wrapped for tmux when running inside it.
///
/// tmux does not forward an OSC it does not implement to the outer terminal
/// unless the sequence is wrapped in its passthrough DCS with every inner ESC
/// doubled. Without the wrapper, copying inside tmux silently does nothing.
fn osc52_clipboard_sequence(encoded: &str, tmux_passthrough: bool) -> Vec<u8> {
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    if !tmux_passthrough {
        return sequence.into_bytes();
    }
    format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b")).into_bytes()
}

fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Write everything queued for the host terminal through the frame's own
/// output, right after the frame that produced it.
pub(super) fn flush_host_terminal_writes(
    terminal: &mut DefaultTerminal,
    pty_runtime: &mut PtyRuntime,
) {
    let bytes = pty_runtime.take_host_terminal_writes();
    if bytes.is_empty() {
        return;
    }
    // A clipboard write must never take down the session: the selection is
    // already made, and the next frame repaints regardless.
    let backend = terminal.backend_mut();
    let _ = backend.write_all(&bytes).and_then(|()| backend.flush());
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
    use crate::app::SelectionCell;
    use crate::model;
    use crate::model::PtyKey;
    use crate::pty::PtyDimensions;
    use crate::runtime::test_support::*;

    #[test]
    fn clipboard_copy_queues_one_sequence_for_the_frame_and_honours_the_opt_out() {
        let mut pty_runtime = PtyRuntime::new_offline();

        assert!(copy_text_to_clipboard(
            &mut pty_runtime,
            &Config::default(),
            "hello"
        ));
        assert_eq!(
            pty_runtime.take_host_terminal_writes(),
            osc52_clipboard_sequence("aGVsbG8=", inside_tmux()),
            "the copy is queued for the frame's output, not written to stdout"
        );
        assert!(
            pty_runtime.take_host_terminal_writes().is_empty(),
            "taking the queue drains it"
        );

        let opted_out = config_with(|config| config.clipboard_osc52 = false);
        assert!(!copy_text_to_clipboard(
            &mut pty_runtime,
            &opted_out,
            "hello"
        ));
        assert!(pty_runtime.take_host_terminal_writes().is_empty());
    }

    #[test]
    fn osc52_is_wrapped_for_tmux_with_doubled_escapes() {
        assert_eq!(
            osc52_clipboard_sequence("aGk=", false),
            b"\x1b]52;c;aGk=\x07".to_vec()
        );
        assert_eq!(
            osc52_clipboard_sequence("aGk=", true),
            b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\".to_vec()
        );
    }

    #[test]
    fn terminal_text_selection_extracts_visible_pane_text() {
        let terminal = PtyKey::Terminal(model::TerminalId(77));
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal, b"abc\r\ndef");

        let selection = TextSelection {
            terminal,
            anchor: SelectionCell { row: 0, col: 1 },
            focus: SelectionCell { row: 1, col: 0 },
            dragging: false,
        };

        assert_eq!(
            selected_text(&mut pty_runtime, selection).as_deref(),
            Some("bc\nd")
        );
    }

    #[test]
    fn text_selection_scrolled_out_of_view_is_copied_whole() {
        let terminal = PtyKey::Terminal(model::TerminalId(79));
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree\r\nfour");

        // The view holds "three"/"four"; "one"/"two" are in the scrollback.
        // Select all four rows the way a drag plus a wheel scroll leaves it:
        // rows are relative to the current view, so the first two are negative.
        let selection = TextSelection {
            terminal,
            anchor: SelectionCell { row: -2, col: 0 },
            focus: SelectionCell { row: 1, col: 3 },
            dragging: false,
        };

        assert_eq!(
            selected_text(&mut pty_runtime, selection).as_deref(),
            Some("one\ntwo\nthree\nfour")
        );
        assert_eq!(
            pty_runtime
                .parser(terminal)
                .expect("parser")
                .screen()
                .scrollback(),
            0,
            "reading the selection leaves the view where it was"
        );
    }

    #[test]
    fn text_selection_older_than_the_scrollback_copies_what_survives() {
        let terminal = PtyKey::Terminal(model::TerminalId(80));
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal, b"one\r\ntwo");

        // Rows -4 and -3 never existed; the selection still yields the rest
        // rather than nothing.
        let selection = TextSelection {
            terminal,
            anchor: SelectionCell { row: -4, col: 0 },
            focus: SelectionCell { row: 1, col: 3 },
            dragging: false,
        };

        assert_eq!(
            selected_text(&mut pty_runtime, selection).as_deref(),
            Some("one\ntwo")
        );
    }

    #[test]
    fn wide_char_text_selection_extracts_expected_cells() {
        let terminal = PtyKey::Terminal(model::TerminalId(78));
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal, PtyDimensions { rows: 1, cols: 8 })
            .expect("resize parser");
        // 'a' at col 0; the wide '你' occupies cols 1-2 (glyph at 1, continuation
        // at 2); 'b' at col 3.
        pty_runtime.process_terminal_output(terminal, "a你b".as_bytes());

        let mut select = |start: u16, end: u16| {
            selected_text(
                &mut pty_runtime,
                TextSelection {
                    terminal,
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
    fn base64_encode_pads_clipboard_payloads() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }
}
