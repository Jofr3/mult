//! Save scheduling: the rate limit that keeps a streaming agent from forcing a
//! full state write every tick, and the exemptions that keep "nothing is lost
//! on exit" true.

use std::time::{Duration, Instant};

use crate::{
    app::App,
    model::{self},
    storage,
};

/// Minimum spacing between two rate-limited state saves (B9).
const MIN_CONTENT_SAVE_INTERVAL: Duration = Duration::from_secs(1);

/// When the last state save ran, so ordinary content saves can be spaced out
/// (B9).
///
/// A save is not cheap — `to_string_pretty` of the whole project, a write, an
/// `fsync`, a rename and a directory `sync` — and streamed agent output dirties
/// the project once per delta, which used to mean one full save per ~16 ms
/// tick. Saves are therefore rate-limited to [`MIN_CONTENT_SAVE_INTERVAL`],
/// with three deliberate exemptions that keep "nothing is lost on exit" true:
/// quitting, a fatal host-terminal error, and any structural change
/// (`App::has_structural_change`).
#[derive(Default)]
pub(super) struct SaveSchedule {
    last_save: Option<Instant>,
}

impl SaveSchedule {
    fn is_due(&self, now: Instant) -> bool {
        self.last_save
            .is_none_or(|last| now.saturating_duration_since(last) >= MIN_CONTENT_SAVE_INTERVAL)
    }

    pub(super) fn record(&mut self, now: Instant) {
        self.last_save = Some(now);
    }
}

/// The rate-limited save the event loop runs every tick (B9). Structural
/// changes ignore the limit; everything else waits for the window.
pub(super) fn save_content_if_due(
    app: &mut App,
    schedule: &mut SaveSchedule,
    now: Instant,
    saver: impl FnMut(&model::ProjectState) -> storage::StateResult<()>,
) -> bool {
    if !app.is_dirty() || (!app.has_structural_change() && !schedule.is_due(now)) {
        return false;
    }
    schedule.record(now);
    save_if_dirty_with(app, false, saver)
}

pub(super) fn save_if_dirty_with(
    app: &mut App,
    force_retry: bool,
    mut saver: impl FnMut(&model::ProjectState) -> storage::StateResult<()>,
) -> bool {
    if !app.is_dirty() || (!force_retry && app.save_error().is_some()) {
        return false;
    }

    match saver(&app.project) {
        Ok(()) => {
            app.mark_saved();
            true
        }
        Err(error) => {
            app.record_save_failure(error.to_string());
            // A normal quit is never allowed to discard dirty state. Return to
            // the TUI and require a fresh quit request after a later retry.
            app.cancel_quit();
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::agent;
    use crate::agent::AgentEvent;
    use crate::model::AgentKind;
    use crate::model::ChatStatus;
    use crate::runtime::finish_after_host_terminal_error_with;
    use std::io;

    #[test]
    fn failed_save_stays_dirty_and_retries_only_when_requested() {
        let mut app = App::default();
        app.add_terminal_to_selected_workspace();
        let attempts = Cell::new(0);
        let mut saver = |_: &model::ProjectState| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(storage::StateError::Io(io::Error::other("disk full")))
            } else {
                Ok(())
            }
        };

        assert!(save_if_dirty_with(&mut app, false, &mut saver));
        assert!(app.is_dirty());
        assert_eq!(app.save_error(), Some("disk full"));
        assert!(!save_if_dirty_with(&mut app, false, &mut saver));
        assert_eq!(attempts.get(), 1);

        assert!(save_if_dirty_with(&mut app, true, &mut saver));
        assert_eq!(attempts.get(), 2);
        assert!(!app.is_dirty());
        assert_eq!(app.save_error(), None);
    }

    /// B9: a save is a full re-serialize plus `fsync`, rename and directory
    /// `sync`, and streamed agent output dirties the project once per delta.
    /// Content saves are therefore spaced out — but a structural change is
    /// never deferred, and a failure is still not retried until asked.
    #[test]
    fn content_saves_are_rate_limited_and_structural_changes_are_not() {
        let mut app = App::default();
        let mut schedule = SaveSchedule::default();
        let start = Instant::now();
        let saves = Cell::new(0);
        let mut saver = |_: &model::ProjectState| {
            saves.set(saves.get() + 1);
            Ok(())
        };

        let workspace = app.project.workspaces[0].id;
        let chat = app
            .project
            .add_chat(
                workspace,
                "agent".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .unwrap()
            .unwrap();
        app.mark_clean();

        let stream_delta = |app: &mut App, text: &str| {
            app.apply_agent_event(AgentEvent::MessageDelta {
                target: agent::AgentTarget { workspace, chat },
                role: agent::AgentMessageRole::Assistant,
                text: text.to_string(),
            });
        };

        stream_delta(&mut app, "first");
        assert!(save_content_if_due(
            &mut app,
            &mut schedule,
            start,
            &mut saver
        ));
        assert_eq!(saves.get(), 1, "the first content change saves at once");

        // Sixty ticks of streamed output inside one second: still one save.
        for tick in 1..=60_u32 {
            stream_delta(&mut app, "more");
            save_content_if_due(
                &mut app,
                &mut schedule,
                start + Duration::from_millis(u64::from(tick) * 16),
                &mut saver,
            );
        }
        assert_eq!(saves.get(), 1, "streamed deltas must not save per delta");
        assert!(app.is_dirty(), "the deferred change is still pending");

        assert!(save_content_if_due(
            &mut app,
            &mut schedule,
            start + MIN_CONTENT_SAVE_INTERVAL,
            &mut saver
        ));
        assert_eq!(saves.get(), 2, "and lands once the window opens");
        assert!(!app.is_dirty());

        // A structural change ignores the window entirely.
        app.add_terminal_to_selected_workspace();
        assert!(save_content_if_due(
            &mut app,
            &mut schedule,
            start + MIN_CONTENT_SAVE_INTERVAL,
            &mut saver
        ));
        assert_eq!(saves.get(), 3);
        assert!(!app.has_structural_change());
    }

    /// B9 (continued): nothing may be lost on the way out. The quit/signal and
    /// fatal-host-terminal paths force a save regardless of the rate limit.
    #[test]
    fn a_deferred_change_is_still_saved_on_the_way_out() {
        let mut app = App::default();
        let mut schedule = SaveSchedule::default();
        let start = Instant::now();
        let saves = Cell::new(0);
        let mut saver = |_: &model::ProjectState| {
            saves.set(saves.get() + 1);
            Ok(())
        };

        app.project.workspaces[0].name = "renamed".to_string();
        app.record_terminal_started(app.project.workspaces[0].terminals[0].id);
        assert!(app.is_dirty());
        schedule.record(start);
        assert!(
            !save_content_if_due(&mut app, &mut schedule, start, &mut saver),
            "the window is closed, so the tick defers"
        );

        finish_after_host_terminal_error_with(
            &mut app,
            io::Error::from_raw_os_error(libc::EIO),
            &mut saver,
        )
        .expect_err("the host-terminal error is still reported");

        assert_eq!(
            saves.get(),
            1,
            "the deferred change is checkpointed on exit"
        );
        assert!(!app.is_dirty());
    }

    #[test]
    fn failed_quit_save_returns_to_tui_and_requires_a_fresh_quit() {
        let mut app = App::default();
        app.add_terminal_to_selected_workspace();
        app.quit();

        assert!(save_if_dirty_with(&mut app, true, |_| {
            Err(storage::StateError::Io(io::Error::other(
                "read-only filesystem",
            )))
        }));
        assert!(!app.should_quit);
        assert!(app.is_dirty());
        assert_eq!(app.save_error(), Some("read-only filesystem"));

        assert!(save_if_dirty_with(&mut app, true, |_| Ok(())));
        assert!(
            !app.should_quit,
            "retry success does not revive the old quit"
        );
        assert!(!app.is_dirty());
        app.quit();
        assert!(app.should_quit);
    }
}
