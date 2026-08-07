use std::io;

use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, DefaultTerminal, Terminal};

/// Owns every process-global terminal mode changed by the TUI.
///
/// Setup and cleanup are deliberately staged: a setup error unwinds only the
/// stages that completed, and cleanup keeps trying after an individual restore
/// operation fails. Clearing each flag before its restore operation also makes
/// explicit cleanup and `Drop` idempotent.
pub(crate) struct TerminalGuard<C: TerminalControl = CrosstermControl> {
    control: C,
    terminal: Option<C::Terminal>,
    raw_mode: bool,
    alternate_screen: bool,
    enhanced_keyboard: bool,
    mouse_capture: bool,
    bracketed_paste: bool,
    focus_change: bool,
}

impl TerminalGuard<CrosstermControl> {
    pub(crate) fn new(mouse_capture: bool) -> io::Result<Self> {
        Self::with_control(CrosstermControl, mouse_capture)
    }
}

impl<C: TerminalControl> TerminalGuard<C> {
    fn with_control(control: C, mouse_capture: bool) -> io::Result<Self> {
        let mut guard = Self {
            control,
            terminal: None,
            raw_mode: false,
            alternate_screen: false,
            enhanced_keyboard: false,
            mouse_capture: false,
            bracketed_paste: false,
            focus_change: false,
        };

        let setup_result: io::Result<()> = (|| {
            guard.control.enable_raw_mode()?;
            guard.raw_mode = true;

            guard.control.enter_alternate_screen()?;
            guard.alternate_screen = true;

            // Preserve Shift on modified keys in terminals that support the
            // kitty keyboard protocol, so Ctrl+Shift+C remains distinct from
            // Ctrl+C.
            guard.control.push_enhanced_keyboard()?;
            guard.enhanced_keyboard = true;

            if mouse_capture {
                guard.control.enable_mouse_capture()?;
                guard.mouse_capture = true;
            }

            guard.control.enable_bracketed_paste()?;
            guard.bracketed_paste = true;

            // A pane's program can ask to be told when it gains or loses focus
            // (DECSET 1004). It only ever has the keyboard when this window
            // does, so the answer has to come from here.
            guard.control.enable_focus_change()?;
            guard.focus_change = true;

            guard.terminal = Some(guard.control.create_terminal()?);
            Ok(())
        })();

        if let Err(setup_error) = setup_result {
            if let Err(cleanup_error) = guard.cleanup() {
                return Err(io::Error::new(
                    setup_error.kind(),
                    format!(
                        "terminal setup failed: {setup_error}; terminal cleanup also failed: {cleanup_error}"
                    ),
                ));
            }
            return Err(setup_error);
        }

        Ok(guard)
    }

    pub(crate) fn terminal_mut(&mut self) -> &mut C::Terminal {
        self.terminal
            .as_mut()
            .expect("terminal is available until the guard is cleaned up")
    }

    pub(crate) fn cleanup(&mut self) -> io::Result<()> {
        let mut first_error = None;

        if let Some(terminal) = self.terminal.take() {
            record_first_error(&mut first_error, self.control.release_terminal(terminal));
        }
        if self.focus_change {
            self.focus_change = false;
            record_first_error(&mut first_error, self.control.disable_focus_change());
        }
        if self.bracketed_paste {
            self.bracketed_paste = false;
            record_first_error(&mut first_error, self.control.disable_bracketed_paste());
        }
        if self.mouse_capture {
            self.mouse_capture = false;
            record_first_error(&mut first_error, self.control.disable_mouse_capture());
        }
        if self.enhanced_keyboard {
            self.enhanced_keyboard = false;
            record_first_error(&mut first_error, self.control.pop_enhanced_keyboard());
        }
        if self.alternate_screen {
            self.alternate_screen = false;
            record_first_error(&mut first_error, self.control.leave_alternate_screen());
        }
        if self.raw_mode {
            self.raw_mode = false;
            record_first_error(&mut first_error, self.control.disable_raw_mode());
        }

        first_error.map_or(Ok(()), Err)
    }
}

impl<C: TerminalControl> Drop for TerminalGuard<C> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn record_first_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result {
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

pub(crate) trait TerminalControl {
    type Terminal;

    fn enable_raw_mode(&mut self) -> io::Result<()>;
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    fn push_enhanced_keyboard(&mut self) -> io::Result<()>;
    fn enable_mouse_capture(&mut self) -> io::Result<()>;
    fn enable_bracketed_paste(&mut self) -> io::Result<()>;
    fn enable_focus_change(&mut self) -> io::Result<()>;
    fn create_terminal(&mut self) -> io::Result<Self::Terminal>;

    fn release_terminal(&mut self, terminal: Self::Terminal) -> io::Result<()>;
    fn disable_focus_change(&mut self) -> io::Result<()>;
    fn disable_bracketed_paste(&mut self) -> io::Result<()>;
    fn disable_mouse_capture(&mut self) -> io::Result<()>;
    fn pop_enhanced_keyboard(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn disable_raw_mode(&mut self) -> io::Result<()>;
}

pub(crate) struct CrosstermControl;

impl TerminalControl for CrosstermControl {
    type Terminal = DefaultTerminal;

    fn enable_raw_mode(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()
    }

    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)
    }

    fn push_enhanced_keyboard(&mut self) -> io::Result<()> {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnableMouseCapture)
    }

    fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnableBracketedPaste)
    }

    fn enable_focus_change(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnableFocusChange)
    }

    fn create_terminal(&mut self) -> io::Result<Self::Terminal> {
        Terminal::new(CrosstermBackend::new(io::stdout()))
    }

    fn release_terminal(&mut self, mut terminal: Self::Terminal) -> io::Result<()> {
        terminal.show_cursor()
    }

    fn disable_focus_change(&mut self) -> io::Result<()> {
        execute!(io::stdout(), DisableFocusChange)
    }

    fn disable_bracketed_paste(&mut self) -> io::Result<()> {
        execute!(io::stdout(), DisableBracketedPaste)
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        execute!(io::stdout(), DisableMouseCapture)
    }

    fn pop_enhanced_keyboard(&mut self) -> io::Result<()> {
        execute!(io::stdout(), PopKeyboardEnhancementFlags)
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        terminal::disable_raw_mode()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        panic::{self, AssertUnwindSafe},
        rc::Rc,
    };

    use super::*;

    const SETUP_STAGES: &[&str] = &[
        "enable_raw_mode",
        "enter_alternate_screen",
        "push_enhanced_keyboard",
        "enable_mouse_capture",
        "enable_bracketed_paste",
        "enable_focus_change",
        "create_terminal",
    ];
    const CLEANUP_STAGES: &[&str] = &[
        "release_terminal",
        "disable_focus_change",
        "disable_bracketed_paste",
        "disable_mouse_capture",
        "pop_enhanced_keyboard",
        "leave_alternate_screen",
        "disable_raw_mode",
    ];

    #[derive(Clone)]
    struct FakeControl {
        events: Rc<RefCell<Vec<&'static str>>>,
        fail_setup: Option<&'static str>,
        fail_cleanup: Vec<&'static str>,
    }

    impl FakeControl {
        fn new(events: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                events,
                fail_setup: None,
                fail_cleanup: Vec::new(),
            }
        }

        fn setup(&self, stage: &'static str) -> io::Result<()> {
            self.events.borrow_mut().push(stage);
            if self.fail_setup == Some(stage) {
                Err(io::Error::other(format!("failed {stage}")))
            } else {
                Ok(())
            }
        }

        fn cleanup(&self, stage: &'static str) -> io::Result<()> {
            self.events.borrow_mut().push(stage);
            if self.fail_cleanup.contains(&stage) {
                Err(io::Error::other(format!("failed {stage}")))
            } else {
                Ok(())
            }
        }
    }

    impl TerminalControl for FakeControl {
        type Terminal = ();

        fn enable_raw_mode(&mut self) -> io::Result<()> {
            self.setup("enable_raw_mode")
        }

        fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.setup("enter_alternate_screen")
        }

        fn push_enhanced_keyboard(&mut self) -> io::Result<()> {
            self.setup("push_enhanced_keyboard")
        }

        fn enable_mouse_capture(&mut self) -> io::Result<()> {
            self.setup("enable_mouse_capture")
        }

        fn enable_bracketed_paste(&mut self) -> io::Result<()> {
            self.setup("enable_bracketed_paste")
        }

        fn enable_focus_change(&mut self) -> io::Result<()> {
            self.setup("enable_focus_change")
        }

        fn create_terminal(&mut self) -> io::Result<Self::Terminal> {
            self.setup("create_terminal")
        }

        fn release_terminal(&mut self, _terminal: Self::Terminal) -> io::Result<()> {
            self.cleanup("release_terminal")
        }

        fn disable_focus_change(&mut self) -> io::Result<()> {
            self.cleanup("disable_focus_change")
        }

        fn disable_bracketed_paste(&mut self) -> io::Result<()> {
            self.cleanup("disable_bracketed_paste")
        }

        fn disable_mouse_capture(&mut self) -> io::Result<()> {
            self.cleanup("disable_mouse_capture")
        }

        fn pop_enhanced_keyboard(&mut self) -> io::Result<()> {
            self.cleanup("pop_enhanced_keyboard")
        }

        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.cleanup("leave_alternate_screen")
        }

        fn disable_raw_mode(&mut self) -> io::Result<()> {
            self.cleanup("disable_raw_mode")
        }
    }

    #[test]
    fn setup_failure_cleans_completed_stages_in_reverse_order() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut control = FakeControl::new(Rc::clone(&events));
        control.fail_setup = Some("create_terminal");

        let error = TerminalGuard::with_control(control, true)
            .err()
            .expect("terminal construction should fail");

        assert_eq!(error.to_string(), "failed create_terminal");
        assert_eq!(
            events.borrow().as_slice(),
            [SETUP_STAGES, &CLEANUP_STAGES[1..],].concat()
        );
    }

    #[test]
    fn cleanup_is_idempotent_and_preserves_the_first_error() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut control = FakeControl::new(Rc::clone(&events));
        control.fail_cleanup = vec!["disable_bracketed_paste", "disable_raw_mode"];
        let mut guard =
            TerminalGuard::with_control(control, true).expect("terminal setup should succeed");

        let error = guard.cleanup().expect_err("cleanup should report an error");
        assert_eq!(error.to_string(), "failed disable_bracketed_paste");
        guard.cleanup().expect("repeated cleanup should do nothing");
        drop(guard);

        assert_eq!(
            events.borrow().as_slice(),
            [SETUP_STAGES, CLEANUP_STAGES].concat()
        );
    }

    #[test]
    fn unwinding_runs_full_cleanup() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let control = FakeControl::new(Rc::clone(&events));

        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard =
                TerminalGuard::with_control(control, true).expect("terminal setup should succeed");
            panic!("test panic");
        }));

        assert!(result.is_err());
        assert_eq!(
            events.borrow().as_slice(),
            [SETUP_STAGES, CLEANUP_STAGES].concat()
        );
    }

    #[test]
    fn mouse_cleanup_is_skipped_when_mouse_capture_is_disabled() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let control = FakeControl::new(Rc::clone(&events));
        let guard =
            TerminalGuard::with_control(control, false).expect("terminal setup should succeed");

        drop(guard);

        let events = events.borrow();
        assert!(!events.contains(&"enable_mouse_capture"));
        assert!(!events.contains(&"disable_mouse_capture"));
    }
}
