//! The status surface: notices with a level, a source and a lifetime.
//!
//! Everything that has no pane to be reported into lands here (E2), which is
//! why a notice carries where it came from: a condition that has ended can
//! retract exactly its own message.

use std::time::{Duration, Instant};

use super::*;
/// How long a transient notice stays on screen. Long enough to read a sentence
/// without looking for it, short enough that a burst of them during a daemon
/// outage does not permanently occupy rows.
pub const NOTICE_TTL: Duration = Duration::from_secs(12);

/// The most notices kept at once. Older ones are dropped rather than growing
/// the surface without bound.
pub const MAX_NOTICES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// Where a notice came from, so a condition that has ended can retract exactly
/// its own message without touching unrelated ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSource {
    /// The state file could not be written. Sticky: it describes a condition
    /// that is still true, and it is retracted by a successful save.
    SaveFailure,
    /// A workspace/chat/terminal mutation failed. Retracted by the next one
    /// that succeeds.
    Operation,
    /// Everything else — connection, protocol, config, state recovery.
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    level: NoticeLevel,
    pub(super) source: NoticeSource,
    text: String,
    /// When the notice stops being rendered, or `None` for a notice describing
    /// a condition that is still true (only [`NoticeSource::SaveFailure`]).
    expires_at: Option<Instant>,
}

impl Notice {
    pub fn level(&self) -> NoticeLevel {
        self.level
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl App {
    pub fn record_save_failure(&mut self, message: impl Into<String>) {
        let message = message.into();
        // Sticky, because it describes a condition that is still true: state is
        // unsaved until a later save succeeds, and `mark_saved` retracts it.
        self.push_sticky_notice(
            NoticeLevel::Error,
            NoticeSource::SaveFailure,
            format!("State save failed: {message} — edit or quit to retry"),
        );
        self.save_error = Some(message);
    }

    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    pub fn record_operation_failure(&mut self, message: impl Into<String>) {
        self.clear_operation_error();
        self.push_notice(
            NoticeLevel::Error,
            NoticeSource::Operation,
            format!("Operation failed: {}", message.into()),
        );
    }

    pub(super) fn clear_operation_error(&mut self) {
        self.notices
            .retain(|notice| notice.source != NoticeSource::Operation);
    }

    /// The transient status surface's current contents, oldest first.
    pub fn notices(&self) -> &[Notice] {
        &self.notices
    }

    /// Report something that has no pane to be reported into (E2).
    pub fn push_notice(
        &mut self,
        level: NoticeLevel,
        source: NoticeSource,
        text: impl Into<String>,
    ) -> bool {
        self.push_notice_at(Instant::now(), level, source, text)
    }

    /// [`Self::push_notice`] with the clock supplied, so tests are deterministic.
    pub fn push_notice_at(
        &mut self,
        now: Instant,
        level: NoticeLevel,
        source: NoticeSource,
        text: impl Into<String>,
    ) -> bool {
        self.insert_notice(Notice {
            level,
            source,
            text: text.into(),
            expires_at: Some(now + NOTICE_TTL),
        })
    }

    fn push_sticky_notice(
        &mut self,
        level: NoticeLevel,
        source: NoticeSource,
        text: impl Into<String>,
    ) -> bool {
        self.insert_notice(Notice {
            level,
            source,
            text: text.into(),
            expires_at: None,
        })
    }

    fn insert_notice(&mut self, notice: Notice) -> bool {
        // A failure that repeats every frame — a retrying reconnect, a save that
        // keeps failing — refreshes the row it already has instead of pushing a
        // fresh copy of the same sentence.
        if let Some(existing) = self
            .notices
            .iter_mut()
            .find(|existing| existing.text == notice.text && existing.level == notice.level)
        {
            existing.expires_at = notice.expires_at;
            existing.source = notice.source;
            // Nothing new is on screen, so this is not a reason to redraw.
            return false;
        }

        self.notices.push(notice);
        let overflow = self.notices.len().saturating_sub(MAX_NOTICES);
        self.notices.drain(..overflow);
        true
    }

    /// Drop notices whose time is up. Returns whether anything went away, so
    /// the render loop only rebuilds a frame when the surface actually changed.
    pub fn expire_notices(&mut self, now: Instant) -> bool {
        let before = self.notices.len();
        self.notices
            .retain(|notice| notice.expires_at.is_none_or(|deadline| deadline > now));
        self.notices.len() != before
    }

    /// Clear the surface on the user's request (`Ctrl+n` / the palette).
    pub fn dismiss_notices(&mut self) -> bool {
        let had_notices = !self.notices.is_empty();
        self.notices.clear();
        had_notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- E2: the status surface -------------------------------------------

    #[test]
    fn notices_are_transient_deduplicated_and_dismissible() {
        let mut app = App::default();
        let now = Instant::now();

        assert!(app.push_notice_at(now, NoticeLevel::Error, NoticeSource::Report, "daemon gone"));
        // A failure that repeats every retry frame refreshes its row rather
        // than filling the surface with copies of one sentence.
        assert!(!app.push_notice_at(now, NoticeLevel::Error, NoticeSource::Report, "daemon gone"));
        assert_eq!(app.notices().len(), 1);
        assert_eq!(app.notices()[0].text(), "daemon gone");
        assert_eq!(app.notices()[0].level(), NoticeLevel::Error);

        // Still there just before the deadline, gone at it: the surface does
        // not permanently steal a row.
        assert!(!app.expire_notices(now + NOTICE_TTL - Duration::from_millis(1)));
        assert_eq!(app.notices().len(), 1);
        assert!(app.expire_notices(now + NOTICE_TTL));
        assert!(app.notices().is_empty());
        assert!(!app.expire_notices(now + NOTICE_TTL));

        // Dismissal does not wait for the deadline.
        app.push_notice_at(
            now,
            NoticeLevel::Info,
            NoticeSource::Report,
            "config reloaded",
        );
        assert!(app.dismiss_notices());
        assert!(app.notices().is_empty());
        assert!(!app.dismiss_notices());
    }

    #[test]
    fn the_notice_surface_is_bounded() {
        let mut app = App::default();
        let now = Instant::now();
        for index in 0..MAX_NOTICES + 3 {
            app.push_notice_at(
                now,
                NoticeLevel::Warning,
                NoticeSource::Report,
                format!("notice {index}"),
            );
        }

        assert_eq!(app.notices().len(), MAX_NOTICES);
        // The oldest are the ones dropped.
        assert_eq!(app.notices()[0].text(), "notice 3");
    }

    #[test]
    fn a_save_failure_notice_sticks_until_a_save_succeeds() {
        let mut app = App::default();
        let now = Instant::now();
        app.record_save_failure("disk full");

        assert_eq!(app.save_error(), Some("disk full"));
        assert_eq!(app.notices().len(), 1);
        assert!(app.notices()[0]
            .text()
            .contains("State save failed: disk full"));
        // It describes a condition that is still true, so time does not clear
        // it the way it clears an event.
        assert!(!app.expire_notices(now + NOTICE_TTL * 100));
        assert_eq!(app.notices().len(), 1);

        app.mark_saved();
        assert_eq!(app.save_error(), None);
        assert!(app.notices().is_empty());
    }

    #[test]
    fn a_failed_operation_is_reported_and_retracted_by_the_next_success() {
        let mut app = App::default();
        app.project.workspaces.clear();
        app.select_nav_index(0);

        // No workspace to add a terminal to.
        app.add_terminal_to_selected_workspace();
        assert_eq!(app.notices().len(), 0, "no workspace means no attempt");

        let mut app = App::default();
        app.record_operation_failure("selected workspace no longer exists");
        assert_eq!(app.notices().len(), 1);
        assert!(app.notices()[0]
            .text()
            .starts_with("Operation failed: selected workspace"));

        app.add_terminal_to_selected_workspace();
        assert!(
            app.notices().is_empty(),
            "a successful mutation retracts the previous failure"
        );
    }

    #[test]
    fn dismiss_notices_is_only_offered_while_there_is_something_to_dismiss() {
        let mut app = App::default();
        let has_dismiss = |app: &App| {
            app.command_palette_entries_for("")
                .iter()
                .any(|entry| entry.action == CommandAction::DismissNotices)
        };

        assert!(!has_dismiss(&app));
        app.push_notice(
            NoticeLevel::Info,
            NoticeSource::Report,
            "something happened",
        );
        assert!(has_dismiss(&app));
    }
}
