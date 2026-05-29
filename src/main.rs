use std::io;

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};
use mult::{app::App, config, storage};

mod runtime;

fn main() -> io::Result<()> {
    let project = storage::load_or_default()?;
    let config = config::load_or_default()?;
    let mouse_capture = config.mouse_capture;
    let mut terminal = ratatui::init();
    if let Err(error) = enable_terminal_features(mouse_capture) {
        ratatui::restore();
        return Err(error);
    }

    let result = runtime::run(&mut terminal, App::new(project), config);
    let terminal_features_result = disable_terminal_features(mouse_capture);
    ratatui::restore();
    result.and(terminal_features_result)
}

fn enable_terminal_features(mouse_capture: bool) -> io::Result<()> {
    // Preserve Shift on modified keys in terminals that support the kitty keyboard
    // protocol, so Ctrl+Shift+C does not arrive looking identical to Ctrl+C.
    if mouse_capture {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            EnableMouseCapture,
            EnableBracketedPaste,
        )
    } else {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            EnableBracketedPaste,
        )
    }
}

fn disable_terminal_features(mouse_capture: bool) -> io::Result<()> {
    if mouse_capture {
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            PopKeyboardEnhancementFlags,
        )
    } else {
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags,
        )
    }
}
