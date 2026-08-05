//! Key encoding for PTY input, and the key predicates the loop classifies
//! keystrokes with.
//!
//! Nothing here touches `App` or the `PtyRuntime`: a `KeyEvent` plus the
//! terminal's cursor-key mode is all it takes to decide what bytes a program
//! behind a PTY should see, which is why it can be read (and tested) without
//! the event loop.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[cfg(test)]
pub(super) fn key_to_pty_bytes(key: KeyEvent) -> Vec<u8> {
    key_to_pty_bytes_in_mode(key, false)
}

pub(super) fn key_to_pty_bytes_in_mode(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
    // Keys that emit their own escape sequence must use xterm's CSI modifier
    // encoding (`CSI 1 ; <mod> <final>` or `CSI <n> ; <mod> ~`) when a modifier
    // is held. Prefixing such a sequence with ESC — the meta convention for
    // plain characters — would send e.g. Alt+Left as `\x1b\x1b[D`, which the PTY
    // application renders as literal characters instead of moving the cursor.
    // Modified cursor keys always use the CSI form, regardless of cursor-key mode.
    if let Some(modifier) = xterm_modifier_code(key.modifiers) {
        if let Some(final_byte) = csi_letter_key(key.code) {
            return format!("\x1b[1;{modifier}{final_byte}").into_bytes();
        }
        if let Some(number) = csi_tilde_key(key.code) {
            return format!("\x1b[{number};{modifier}~").into_bytes();
        }
    }

    let Some(mut bytes) = base_key_to_pty_bytes(key, application_cursor) else {
        return Vec::new();
    };

    // Meta convention: Alt+<key> is the base byte(s) prefixed with ESC, e.g.
    // Alt+b -> `\x1bb`, Alt+Backspace -> `\x1b\x7f` (delete previous word).
    if key.modifiers.contains(KeyModifiers::ALT) {
        let mut prefixed = Vec::with_capacity(bytes.len() + 1);
        prefixed.push(0x1b);
        prefixed.append(&mut bytes);
        prefixed
    } else {
        bytes
    }
}

/// xterm modifier parameter for CSI-encoded keys: `1` plus a bitmask of
/// Shift (1), Alt (2), and Ctrl (4). Returns `None` when none of those are held
/// so that unmodified keys keep their plain escape sequence.
fn xterm_modifier_code(modifiers: KeyModifiers) -> Option<u8> {
    let mut bits = 0u8;
    if modifiers.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    (bits != 0).then_some(bits + 1)
}

/// Unmodified cursor-key sequence: SS3 (`ESC O <final>`) when the application
/// has enabled DECCKM (e.g. vim, less, fzf), CSI (`ESC [ <final>`) otherwise.
fn cursor_key_bytes(application_cursor: bool, final_byte: char) -> Vec<u8> {
    let introducer = if application_cursor { "\x1bO" } else { "\x1b[" };
    format!("{introducer}{final_byte}").into_bytes()
}

/// Final byte for keys encoded as `CSI 1 ; <mod> <final>` when modified:
/// arrows, Home/End, and F1–F4.
fn csi_letter_key(code: KeyCode) -> Option<char> {
    Some(match code {
        KeyCode::Up => 'A',
        KeyCode::Down => 'B',
        KeyCode::Right => 'C',
        KeyCode::Left => 'D',
        KeyCode::Home => 'H',
        KeyCode::End => 'F',
        KeyCode::F(1) => 'P',
        KeyCode::F(2) => 'Q',
        KeyCode::F(3) => 'R',
        KeyCode::F(4) => 'S',
        _ => return None,
    })
}

/// Leading number for keys encoded as `CSI <number> ; <mod> ~` when modified:
/// Insert/Delete, Page Up/Down, and F5–F12. The numbers mirror the plain
/// sequences in [`base_key_to_pty_bytes`].
fn csi_tilde_key(code: KeyCode) -> Option<u8> {
    Some(match code {
        KeyCode::Insert => 2,
        KeyCode::Delete => 3,
        KeyCode::PageUp => 5,
        KeyCode::PageDown => 6,
        KeyCode::F(5) => 15,
        KeyCode::F(6) => 17,
        KeyCode::F(7) => 18,
        KeyCode::F(8) => 19,
        KeyCode::F(9) => 20,
        KeyCode::F(10) => 21,
        KeyCode::F(11) => 23,
        KeyCode::F(12) => 24,
        _ => return None,
    })
}

fn base_key_to_pty_bytes(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    Some(match key.code {
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Left => cursor_key_bytes(application_cursor, 'D'),
        KeyCode::Right => cursor_key_bytes(application_cursor, 'C'),
        KeyCode::Up => cursor_key_bytes(application_cursor, 'A'),
        KeyCode::Down => cursor_key_bytes(application_cursor, 'B'),
        KeyCode::Home => cursor_key_bytes(application_cursor, 'H'),
        KeyCode::End => cursor_key_bytes(application_cursor, 'F'),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        // Never collapse Ctrl+Shift+C into ETX/Ctrl+C when enhanced keyboard
        // reporting lets us tell those keypresses apart.
        KeyCode::Char(_) if is_shifted_control_char(key, 'c') => return None,
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(5) => b"\x1b[15~".to_vec(),
        KeyCode::F(6) => b"\x1b[17~".to_vec(),
        KeyCode::F(7) => b"\x1b[18~".to_vec(),
        KeyCode::F(8) => b"\x1b[19~".to_vec(),
        KeyCode::F(9) => b"\x1b[20~".to_vec(),
        KeyCode::F(10) => b"\x1b[21~".to_vec(),
        KeyCode::F(11) => b"\x1b[23~".to_vec(),
        KeyCode::F(12) => b"\x1b[24~".to_vec(),
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            vec![control_byte(c)?]
        }
        // Under the Kitty disambiguate protocol the host reports Shift combined
        // with Alt/Super as the unshifted base key plus a separate Shift bit
        // (e.g. Alt+Shift+h -> Char('h') + SHIFT|ALT) instead of folding Shift
        // into the glyph the way a legacy terminal does. Fold it back in here so
        // the shifted character reaches the PTY; otherwise the modifier is
        // dropped and Alt+Shift+h is indistinguishable from Alt+h to a legacy
        // app like vim. (Ctrl+Shift is handled above, where Shift never changes
        // the control byte.)
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            c.to_uppercase().to_string().into_bytes()
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        _ => return None,
    })
}

fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

pub(super) fn is_quit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc) && is_control_key(key)
}
pub(super) fn is_control_down_key(key: KeyEvent) -> bool {
    is_unshifted_control_char(key, 'j')
        || (matches!(key.code, KeyCode::Enter) && is_control_key(key))
}

pub(super) fn is_control_up_key(key: KeyEvent) -> bool {
    is_unshifted_control_char(key, 'k')
}

pub(super) fn is_unshifted_control_char(key: KeyEvent, target: char) -> bool {
    let KeyCode::Char(ch) = key.code else {
        return false;
    };

    is_control_key(key)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && ch == target.to_ascii_lowercase()
}

pub(super) fn is_shifted_control_char(key: KeyEvent, target: char) -> bool {
    let KeyCode::Char(ch) = key.code else {
        return false;
    };

    is_control_key(key)
        && key.modifiers.contains(KeyModifiers::SHIFT)
        && ch.eq_ignore_ascii_case(&target)
}

pub(super) fn is_control_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_key_bytes_encode_printable_text() {
        let key = KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE);

        assert_eq!(key_to_pty_bytes(key), "é".as_bytes());
    }

    #[test]
    fn terminal_key_bytes_encode_control_keys() {
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![0x03]
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            vec![0x7f]
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            b"\r".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            b"\x1b".to_vec()
        );
    }

    #[test]
    fn terminal_key_bytes_encode_navigation_and_alt_keys() {
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            b"\x1b[A".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
            b"\x1bx".to_vec()
        );
    }

    #[test]
    fn alt_shift_letters_fold_shift_into_uppercase() {
        // Regression: under the Kitty disambiguate protocol crossterm reports
        // Alt+Shift+h as Char('h') + SHIFT|ALT (the unshifted base key). Shift
        // must survive as an uppercase glyph so the PTY sees `ESC H` (<M-H>), not
        // `ESC h` (<M-h>) — otherwise Alt+Shift+h/j/k/l collapse onto
        // Alt+h/j/k/l inside vim.
        for (lower, upper) in [('h', 'H'), ('j', 'J'), ('k', 'K'), ('l', 'L')] {
            assert_eq!(
                key_to_pty_bytes(KeyEvent::new(
                    KeyCode::Char(lower),
                    KeyModifiers::ALT | KeyModifiers::SHIFT,
                )),
                vec![0x1b, upper as u8],
                "Alt+Shift+{lower} must encode as ESC {upper}",
            );
        }
    }

    #[test]
    fn alt_arrow_keys_use_csi_modifier_encoding() {
        // Regression: Alt+Arrow must move the cursor via `CSI 1 ; 3 <dir>`, not
        // arrive as a doubled-ESC sequence that the PTY renders as characters.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            b"\x1b[1;3D".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
            b"\x1b[1;3C".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
            b"\x1b[1;3A".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
            b"\x1b[1;3B".to_vec()
        );
    }

    #[test]
    fn ctrl_and_shift_arrows_use_csi_modifier_encoding() {
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            b"\x1b[1;5D".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT)),
            b"\x1b[1;2C".to_vec()
        );
        // Combined modifiers follow the xterm bitmask: 1 + shift + alt*2 + ctrl*4.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            b"\x1b[1;7A".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(
                KeyCode::End,
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            )),
            b"\x1b[1;8F".to_vec()
        );
    }

    #[test]
    fn modified_home_paging_and_function_keys_encode_modifiers() {
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL)),
            b"\x1b[1;5H".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL)),
            b"\x1b[3;5~".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT)),
            b"\x1b[5;2~".to_vec()
        );
        // F1–F4 switch from SS3 to CSI form once modified; F5+ keep the tilde form.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::F(1), KeyModifiers::SHIFT)),
            b"\x1b[1;2P".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::F(5), KeyModifiers::CONTROL)),
            b"\x1b[15;5~".to_vec()
        );
    }

    #[test]
    fn unmodified_navigation_keys_keep_plain_sequences() {
        // Without a modifier there are no CSI parameters, matching every VT100 app.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            b"\x1b[D".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
            b"\x1b[3~".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            b"\x1bOP".to_vec()
        );
    }

    #[test]
    fn alt_simple_keys_still_use_meta_escape_prefix() {
        // The meta convention stays correct for printable characters and keys
        // whose base encoding is a single control byte.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)),
            b"\x1bb".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
            b"\x1b\x7f".to_vec()
        );
    }

    #[test]
    fn application_cursor_mode_uses_ss3_for_unmodified_cursor_keys() {
        // DECCKM: full-screen apps (vim, less, fzf) expect SS3 (`ESC O <dir>`)
        // arrows rather than the CSI (`ESC [ <dir>`) form used by the shell.
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true),
            b"\x1bOA".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), true),
            b"\x1bOD".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), true),
            b"\x1bOH".to_vec()
        );
    }

    #[test]
    fn application_cursor_mode_keeps_csi_for_modified_and_non_cursor_keys() {
        // A held modifier always selects the CSI form, even under DECCKM.
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), true),
            b"\x1b[1;3A".to_vec()
        );
        // Paging keys are not cursor keys, so DECCKM leaves them untouched.
        assert_eq!(
            key_to_pty_bytes_in_mode(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), true),
            b"\x1b[6~".to_vec()
        );
    }
}
