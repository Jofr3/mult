//! The global status line's queue.
//!
//! Everything without a pane of its own ends up here: a save that could not be
//! written, a frame that could not be drawn, a daemon that could not be
//! reached, the config warnings from startup, the state-backup notice (E2).

use super::App;

/// Upper bound on undismissed notices. Startup can legitimately produce several
/// (a config warning per bad colour key, plus a state-backup notice); beyond
/// that the oldest is dropped rather than grown without limit.
const MAX_STATUS_NOTICES: usize = 8;

/// How loudly a [`StatusNotice`] is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    /// Something happened that is worth confirming — a config reloaded, a state
    /// file moved aside.
    Info,
    /// Something worked but not as configured — a colour key that did not parse.
    Warning,
    /// Something failed — a save, a frame, the daemon connection.
    Error,
}

impl StatusLevel {
    /// Leading marker. A shape, not only a colour, so the levels stay
    /// distinguishable under `NO_COLOR` and for readers who cannot tell the
    /// palette's love from its gold.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Info => "· ",
            Self::Warning => "! ",
            Self::Error => "x ",
        }
    }
}

/// A dismissible message in the global status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusNotice {
    pub level: StatusLevel,
    pub message: String,
}

impl App {
    /// The notice currently shown in the status line, if any. Oldest first, so
    /// a burst of startup warnings is read in the order it was produced.
    pub fn current_status_notice(&self) -> Option<&StatusNotice> {
        self.status_notices.front()
    }

    /// How many notices are waiting behind the current one.
    pub fn queued_status_notice_count(&self) -> usize {
        self.status_notices.len().saturating_sub(1)
    }

    /// Dismiss the current notice, revealing the next. Returns whether anything
    /// was dismissed, so the caller only redraws when the screen changed.
    pub fn dismiss_status_notice(&mut self) -> bool {
        self.status_notices.pop_front().is_some()
    }

    /// Queue a non-fatal problem for the status line.
    ///
    /// Deduplicated against everything already queued: the failures that reach
    /// here are mostly per-tick (a save that keeps failing, a frame that keeps
    /// not drawing), and a queue that grew one entry per tick would be a memory
    /// leak the user has to press a key 60 times a second to drain.
    pub fn push_status_notice(&mut self, level: StatusLevel, message: impl Into<String>) {
        let notice = StatusNotice {
            level,
            message: message.into(),
        };
        if self.status_notices.contains(&notice) {
            return;
        }
        if self.status_notices.len() >= MAX_STATUS_NOTICES {
            self.status_notices.pop_front();
        }
        self.status_notices.push_back(notice);
    }

    /// Report a non-fatal runtime failure (a save that could not be written, a
    /// frame that could not be drawn, a daemon that could not be reached).
    pub fn set_last_error(&mut self, message: impl Into<String>) {
        self.push_status_notice(StatusLevel::Error, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_notices_are_shown_oldest_first_and_dismissed_one_at_a_time() {
        let mut app = App::two_workspaces();
        assert!(app.current_status_notice().is_none());

        app.push_status_notice(StatusLevel::Warning, "first");
        app.set_last_error("second");

        let notice = app.current_status_notice().expect("a notice is shown");
        assert_eq!(notice.level, StatusLevel::Warning);
        assert_eq!(notice.message, "first");
        assert_eq!(app.queued_status_notice_count(), 1);

        assert!(app.dismiss_status_notice());
        let notice = app.current_status_notice().expect("the next notice");
        assert_eq!(notice.level, StatusLevel::Error);
        assert_eq!(notice.message, "second");
        assert_eq!(app.queued_status_notice_count(), 0);

        assert!(app.dismiss_status_notice());
        assert!(app.current_status_notice().is_none());
        // Dismissing nothing is not a change, so the loop does not redraw.
        assert!(!app.dismiss_status_notice());
    }

    #[test]
    fn repeated_failures_neither_flood_nor_grow_the_notice_queue() {
        // The failures that reach the status line are mostly per-tick: a save
        // that keeps failing reports once per loop iteration.
        let mut app = App::two_workspaces();
        for _ in 0..1_000 {
            app.set_last_error("failed to save state: No space left on device");
        }

        assert_eq!(app.queued_status_notice_count(), 0);

        for index in 0..MAX_STATUS_NOTICES * 2 {
            app.push_status_notice(StatusLevel::Warning, format!("distinct {index}"));
        }

        assert_eq!(app.queued_status_notice_count(), MAX_STATUS_NOTICES - 1);
    }
}
