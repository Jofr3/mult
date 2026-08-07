//! The key → PTY byte encoder, and the control-key predicates the client uses
//! to recognise its own shortcuts.
//!
//! This is pure translation: no `App`, no `PtyRuntime`, nothing to mock.

use crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers};

/// The input modes a PTY program has switched on, which change what bytes a
/// key must produce.
///
/// Both default to off, which is the state of a freshly opened terminal and the
/// state a shell leaves it in; a full-screen program sets one or both and is
/// entitled to expect the encoding it asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct PtyKeyModes {
    /// DECCKM: cursor keys arrive as SS3 (`ESC O A`) rather than CSI.
    pub(super) application_cursor: bool,
    /// DECKPAM: the numeric keypad sends its own SS3 sequences rather than the
    /// digits and operators printed on the keys.
    pub(super) application_keypad: bool,
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

fn is_control_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT)
}

#[cfg(test)]
pub(super) fn key_to_pty_bytes(key: KeyEvent) -> Vec<u8> {
    key_to_pty_bytes_in_mode(key, PtyKeyModes::default())
}

pub(super) fn key_to_pty_bytes_in_mode(key: KeyEvent, modes: PtyKeyModes) -> Vec<u8> {
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

    let Some(mut bytes) = base_key_to_pty_bytes(key, modes) else {
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

fn base_key_to_pty_bytes(key: KeyEvent, modes: PtyKeyModes) -> Option<Vec<u8>> {
    if let Some(bytes) = application_keypad_bytes(key, modes) {
        return Some(bytes);
    }

    let application_cursor = modes.application_cursor;
    Some(match key.code {
        // xterm's Ctrl+Backspace is BS, distinct from the DEL that Backspace
        // alone sends; readline and every shell bind the two separately
        // (delete character vs. delete word).
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => vec![0x08],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Left => cursor_key_bytes(application_cursor, 'D'),
        KeyCode::Right => cursor_key_bytes(application_cursor, 'C'),
        KeyCode::Up => cursor_key_bytes(application_cursor, 'A'),
        KeyCode::Down => cursor_key_bytes(application_cursor, 'B'),
        KeyCode::Home => cursor_key_bytes(application_cursor, 'H'),
        KeyCode::End => cursor_key_bytes(application_cursor, 'F'),
        // The keypad's centre key has no glyph to fall back on, so it keeps its
        // CSI form outside application-keypad mode.
        KeyCode::KeypadBegin => b"\x1b[E".to_vec(),
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
            match control_byte(c) {
                Some(byte) => vec![byte],
                // A Ctrl combination with no control code of its own (Ctrl+1,
                // Ctrl+9, Ctrl+;) sends the bare character in xterm. Returning
                // `None` here instead would swallow the keypress: the user would
                // press a key and watch nothing happen.
                None => c.to_string().into_bytes(),
            }
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

/// The control code xterm sends for `Ctrl+<c>`, or `None` when the combination
/// has none.
///
/// The digit rows exist because the control codes below `Ctrl+A` have no letter
/// to be built from, so terminals have always reached them through the digits
/// and punctuation that share a key with the symbol: `Ctrl+2` and `Ctrl+@` are
/// both NUL, `Ctrl+6` and `Ctrl+^` are both RS, and `Ctrl+/` is US. Losing them
/// is not cosmetic — `Ctrl+_` is undo in readline and emacs, and `Ctrl+@` is
/// set-mark.
fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | ' ' | '2' => Some(0x00),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// The SS3 sequence a keypad key sends while the program has DECKPAM on.
///
/// Only unmodified presses take this path: a held modifier selects the CSI or
/// meta encoding as it does everywhere else, and no terminal has ever had an
/// application-keypad form for a modified key. `None` means "not a keypad key,
/// or the program never asked", and the ordinary encoding applies — which for
/// every key here is the glyph printed on it.
///
/// Recognising the keypad at all depends on the host terminal telling us it was
/// the keypad (the kitty protocol's `KEYPAD` state, which
/// [`crate::terminal_guard`] asks for); a terminal that does not report it
/// simply keeps the numeric encoding, which is what it would have sent anyway.
fn application_keypad_bytes(key: KeyEvent, modes: PtyKeyModes) -> Option<Vec<u8>> {
    if !modes.application_keypad
        || !key.state.contains(KeyEventState::KEYPAD)
        || key.modifiers.intersects(
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER,
        )
    {
        return None;
    }

    let final_byte = match key.code {
        KeyCode::Char('0') => 'p',
        KeyCode::Char('1') => 'q',
        KeyCode::Char('2') => 'r',
        KeyCode::Char('3') => 's',
        KeyCode::Char('4') => 't',
        KeyCode::Char('5') => 'u',
        KeyCode::Char('6') => 'v',
        KeyCode::Char('7') => 'w',
        KeyCode::Char('8') => 'x',
        KeyCode::Char('9') => 'y',
        KeyCode::Char('*') => 'j',
        KeyCode::Char('+') => 'k',
        KeyCode::Char(',') => 'l',
        KeyCode::Char('-') => 'm',
        KeyCode::Char('.') => 'n',
        KeyCode::Char('/') => 'o',
        KeyCode::Char('=') => 'X',
        KeyCode::Enter => 'M',
        KeyCode::KeypadBegin => 'E',
        _ => return None,
    };
    Some(format!("\x1bO{final_byte}").into_bytes())
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

    const APPLICATION_CURSOR: PtyKeyModes = PtyKeyModes {
        application_cursor: true,
        application_keypad: false,
    };
    const APPLICATION_KEYPAD: PtyKeyModes = PtyKeyModes {
        application_cursor: false,
        application_keypad: true,
    };

    /// A keypad press, as the host reports one under the kitty protocol: the
    /// glyph printed on the key plus the `KEYPAD` state bit.
    fn keypad(code: KeyCode) -> KeyEvent {
        let mut key = KeyEvent::new(code, KeyModifiers::NONE);
        key.state = KeyEventState::KEYPAD;
        key
    }

    #[test]
    fn application_cursor_mode_uses_ss3_for_unmodified_cursor_keys() {
        // DECCKM: full-screen apps (vim, less, fzf) expect SS3 (`ESC O <dir>`)
        // arrows rather than the CSI (`ESC [ <dir>`) form used by the shell.
        assert_eq!(
            key_to_pty_bytes_in_mode(
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                APPLICATION_CURSOR
            ),
            b"\x1bOA".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes_in_mode(
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                APPLICATION_CURSOR
            ),
            b"\x1bOD".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes_in_mode(
                KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                APPLICATION_CURSOR
            ),
            b"\x1bOH".to_vec()
        );
    }

    #[test]
    fn application_cursor_mode_keeps_csi_for_modified_and_non_cursor_keys() {
        // A held modifier always selects the CSI form, even under DECCKM.
        assert_eq!(
            key_to_pty_bytes_in_mode(
                KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
                APPLICATION_CURSOR
            ),
            b"\x1b[1;3A".to_vec()
        );
        // Paging keys are not cursor keys, so DECCKM leaves them untouched.
        assert_eq!(
            key_to_pty_bytes_in_mode(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                APPLICATION_CURSOR
            ),
            b"\x1b[6~".to_vec()
        );
    }

    #[test]
    fn application_keypad_mode_sends_ss3_for_the_keypad_only() {
        // DECKPAM: the keypad stops sending its glyphs and sends its own SS3
        // sequences, which is how a program tells `Enter` from `KP_Enter` and
        // `5` from `KP_5`.
        for (code, expected) in [
            (KeyCode::Char('0'), "\x1bOp"),
            (KeyCode::Char('5'), "\x1bOu"),
            (KeyCode::Char('9'), "\x1bOy"),
            (KeyCode::Char('.'), "\x1bOn"),
            (KeyCode::Char('+'), "\x1bOk"),
            (KeyCode::Char('-'), "\x1bOm"),
            (KeyCode::Char('*'), "\x1bOj"),
            (KeyCode::Char('/'), "\x1bOo"),
            (KeyCode::Enter, "\x1bOM"),
            (KeyCode::KeypadBegin, "\x1bOE"),
        ] {
            assert_eq!(
                key_to_pty_bytes_in_mode(keypad(code), APPLICATION_KEYPAD),
                expected.as_bytes(),
                "keypad {code:?} under DECKPAM",
            );
        }

        // The same keys on the main keyboard are untouched: they carry no
        // `KEYPAD` state, so nothing distinguishes them from ordinary typing.
        assert_eq!(
            key_to_pty_bytes_in_mode(
                KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE),
                APPLICATION_KEYPAD
            ),
            b"5".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes_in_mode(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                APPLICATION_KEYPAD
            ),
            b"\r".to_vec()
        );
    }

    #[test]
    fn the_keypad_sends_its_glyphs_when_the_program_did_not_ask() {
        // Without DECKPAM the keypad is just keys: a program that never asked
        // must not be handed sequences it will render as garbage.
        assert_eq!(
            key_to_pty_bytes_in_mode(keypad(KeyCode::Char('5')), PtyKeyModes::default()),
            b"5".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes_in_mode(keypad(KeyCode::Enter), PtyKeyModes::default()),
            b"\r".to_vec()
        );
        // A modifier selects the ordinary encoding even under DECKPAM — no
        // terminal has an application-keypad form for a modified key.
        let mut ctrl_keypad = keypad(KeyCode::Char('5'));
        ctrl_keypad.modifiers = KeyModifiers::CONTROL;
        assert_eq!(
            key_to_pty_bytes_in_mode(ctrl_keypad, APPLICATION_KEYPAD),
            vec![0x1d]
        );
    }

    #[test]
    fn control_digits_and_punctuation_reach_their_control_codes() {
        // The control codes below Ctrl+A have no letter to be built from, so
        // terminals reach them through the digits and punctuation sharing a key
        // with the symbol. `Ctrl+_` is undo in readline; losing it is not
        // cosmetic.
        for (ch, expected) in [
            ('2', 0x00),
            ('@', 0x00),
            (' ', 0x00),
            ('3', 0x1b),
            ('[', 0x1b),
            ('4', 0x1c),
            ('5', 0x1d),
            ('6', 0x1e),
            ('^', 0x1e),
            ('7', 0x1f),
            ('_', 0x1f),
            ('/', 0x1f),
            ('8', 0x7f),
            ('?', 0x7f),
        ] {
            assert_eq!(
                key_to_pty_bytes(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)),
                vec![expected],
                "Ctrl+{ch}",
            );
        }
    }

    #[test]
    fn a_control_combination_without_a_control_code_still_sends_its_character() {
        // xterm sends the bare character for Ctrl+1 or Ctrl+9. Sending nothing
        // — which is what a missing table entry used to mean — is a keypress
        // the user watches vanish.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL)),
            b"1".to_vec()
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::CONTROL)),
            b"9".to_vec()
        );
        // The one deliberate exception stays: Ctrl+Shift+C is `mult`'s copy
        // shortcut and must never reach the PTY as anything.
        assert!(key_to_pty_bytes(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .is_empty());
    }

    #[test]
    fn ctrl_backspace_is_distinct_from_backspace() {
        // Backspace is DEL and Ctrl+Backspace is BS; readline binds them to
        // delete-char and delete-word, so collapsing them loses word deletion.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            vec![0x7f]
        );
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)),
            vec![0x08]
        );
        // Alt still prefixes whichever of the two it is applied to.
        assert_eq!(
            key_to_pty_bytes(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            vec![0x1b, 0x08]
        );
    }
}
