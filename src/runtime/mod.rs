//! Runtime orchestration for the `mult` client: the event loop and the wiring
//! that drives `App`, the `PtyRuntime` and the agent backends. `main.rs` keeps
//! only terminal setup/teardown and calls [`run`].
//!
//! The loop itself is here; everything it delegates to lives in a submodule —
//! [`input`] (keys and prompts), [`keymap`] (key → PTY bytes), [`mouse`]
//! (hit-testing), [`clipboard`] (OSC 52), [`session`] (starting and sizing
//! panes), [`agent_launch`] (agent command lines and their generated runtime
//! files) and [`agent_status`] (the status poller and its source seam).

mod agent_launch;
mod agent_status;
mod clipboard;
mod input;
mod keymap;
mod mouse;
mod prompts;
mod session;

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crossterm::event;
use mult_protocol::InstanceId;
use ratatui::{layout::Rect, DefaultTerminal};

use crate::{
    app::{App, StatusLevel},
    config::{self, Config},
    git,
    layout::AppLayout,
    model::{ChatStatus, PtyKey},
    pty::{PtyEvent, PtyRuntime},
    storage::{self, StateStore},
    ui,
};

use self::{
    agent_status::{
        drain_mult_agent_status_events, mult_agent_status_path, remove_agent_status_files,
        AgentStatusPoller, FileAgentStatusSource,
    },
    clipboard::flush_pending_clipboard,
    input::handle_event,
    session::{
        auto_start_selected_chat_agent, auto_start_selected_terminal, chat_agent_kind,
        resize_visible_chat_agent, resize_visible_terminal, restore_persisted_sessions,
    },
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const READY_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(0);
const GIT_BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Poll interval once the loop has been idle for `IDLE_TICKS_BEFORE_BACKOFF`
/// ticks. See [`IdleBackoff`].
const IDLE_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Consecutive ticks with nothing to do before the poll interval backs off
/// (~0.5 s at `EVENT_POLL_INTERVAL`).
const IDLE_TICKS_BEFORE_BACKOFF: u32 = 30;
/// Rate limit for ordinary state saves. A save re-serializes the whole project
/// and `fsync`s twice (temp file, then parent directory), while streamed agent
/// output marks the state dirty on nearly every frame — so an unthrottled save
/// meant two fsyncs per frame over an ever-growing JSON blob.
const SAVE_INTERVAL: Duration = Duration::from_secs(1);
/// Consecutive failed frame draws tolerated before the session gives up. A
/// single failure is usually transient (a resize race, a full output buffer);
/// a terminal that will not accept any frame cannot be driven at all.
const MAX_CONSECUTIVE_DRAW_ERRORS: u32 = 3;

/// Everything the loop needs that is not state: where the config it may reload
/// lives, and which daemon socket to use.
pub struct RuntimeOptions {
    /// The config file in force, so "Reload config" re-reads the same file the
    /// session started from — including one named by `--config` (E1/E9).
    pub config_path: PathBuf,
    /// `--socket`, or `None` to let `$MULT_SOCKET_PATH` and the default decide.
    pub socket_path: Option<PathBuf>,
}

pub fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut config: Config,
    store: &mut dyn StateStore,
    options: RuntimeOptions,
) -> io::Result<()> {
    // Allocated before anything connects and persisted with the project, so the
    // daemon can tell this `mult` from any other one sharing its socket (A3).
    let instance = InstanceId(app.ensure_instance_token());
    let mut pty_runtime = PtyRuntime::autospawning(options.socket_path, instance);
    let size = terminal.size()?;
    // The frame's geometry, resolved once and shared by the renderer, the
    // resize handlers and mouse hit-testing (F6). It is refreshed immediately
    // before each draw, so it always describes the frame that is on screen —
    // exactly the role the raw `frame_area` used to play, minus the four
    // independent recomputations per iteration.
    let mut layout = AppLayout::compute(&app, Rect::new(0, 0, size.width, size.height));
    restore_persisted_sessions(&mut app, &mut pty_runtime, &config, &layout);
    // Branch probes fork `git` per workspace, which costs milliseconds each and
    // used to run on this thread; the watcher answers over a channel instead.
    let mut branch_watcher = git::BranchWatcher::new(git::GitBranchProbe);
    request_workspace_git_branches(&app, &mut branch_watcher);
    let mut last_git_branch_refresh = Instant::now();
    let mut status_poller = AgentStatusPoller::new(FileAgentStatusSource::new());
    let mut idle = IdleBackoff::default();
    let mut last_save = Instant::now();
    let mut consecutive_draw_errors = 0_u32;
    let mut fatal: Option<io::Error> = None;

    // The screen is static unless something changes, so only rebuild a frame
    // when needed instead of every ~16ms tick. The tick still runs so PTY/agent
    // output (delivered over channels, not via event::poll) is drained promptly;
    // it is just the expensive draw that is gated. `needs_redraw` is set by any
    // input event, drained PTY/agent/status change, git-branch refresh, or an
    // auto-start/resize that altered state.
    let mut needs_redraw = true;
    while !app.should_quit {
        let now = Instant::now();
        if now.duration_since(last_git_branch_refresh) >= GIT_BRANCH_REFRESH_INTERVAL {
            request_workspace_git_branches(&app, &mut branch_watcher);
            last_git_branch_refresh = now;
        }
        if let Some(branches) = branch_watcher.poll() {
            needs_redraw |= app.replace_workspace_git_branches(branches);
        }
        needs_redraw |= drain_pty_events(&mut app, &mut pty_runtime);
        needs_redraw |=
            drain_mult_agent_status_events(&mut app, &pty_runtime, &mut status_poller, now);
        // A save that cannot be written (full disk, read-only data directory)
        // is reported and retried, never fatal: killing the session would throw
        // away the very state that failed to persist.
        if let Err(error) = save_if_dirty(&mut app, store, &mut last_save, now, false) {
            app.set_last_error(format!("failed to save state: {error}"));
        }
        needs_redraw |= resize_visible_terminal(&mut pty_runtime, &layout);
        needs_redraw |= resize_visible_chat_agent(&mut pty_runtime, &layout);
        needs_redraw |= auto_start_selected_terminal(&mut app, &mut pty_runtime, &config, &layout);
        needs_redraw |=
            auto_start_selected_chat_agent(&mut app, &mut pty_runtime, &config, &layout);

        // Anything that asks for a redraw counts as activity, so the loop only
        // backs off once nothing at all is happening.
        if needs_redraw {
            idle.record_activity();
        } else {
            idle.record_idle_tick();
        }

        if needs_redraw {
            // Resolved from the live terminal size rather than from inside the
            // draw closure, so the renderer and the handlers below share one
            // value instead of the renderer owning the only accurate one. A
            // failed query keeps the last known frame rather than inventing a
            // size.
            layout = AppLayout::compute(&app, terminal_frame_area(terminal, layout.frame));
            match terminal.draw(|frame| ui::draw(frame, &layout, &app, &pty_runtime, &config)) {
                Ok(completed) => {
                    // A resize that landed between the size query and the draw
                    // leaves the frame laid out for the previous size; ask for
                    // one more frame rather than waiting for the next event.
                    needs_redraw = completed.area != layout.frame;
                    consecutive_draw_errors = 0;
                }
                Err(error) => {
                    // `needs_redraw` stays set so the next tick retries. Repeated
                    // failures mean the output is gone, which no retry fixes.
                    consecutive_draw_errors += 1;
                    if consecutive_draw_errors >= MAX_CONSECUTIVE_DRAW_ERRORS {
                        fatal = Some(error);
                        break;
                    }
                    app.set_last_error(format!("failed to draw frame: {error}"));
                }
            }
        }

        // After the frame, on the same writer: a copy is a side channel to the
        // host terminal, not part of the rendered screen, and a failed clipboard
        // write is never worth ending the session over.
        if let Err(error) = flush_pending_clipboard(terminal, &mut app) {
            app.set_last_error(format!("failed to copy to the clipboard: {error}"));
        }

        if event::poll(idle.poll_interval())? {
            handle_event(&mut app, &mut pty_runtime, &config, event::read()?, &layout);
            needs_redraw = true;
            idle.record_activity();
            while !app.should_quit && event::poll(READY_EVENT_POLL_INTERVAL)? {
                handle_event(&mut app, &mut pty_runtime, &config, event::read()?, &layout);
            }
            if let Err(error) =
                save_if_dirty(&mut app, store, &mut last_save, Instant::now(), false)
            {
                app.set_last_error(format!("failed to save state: {error}"));
            }
            // Outside the borrow of `config` that the handlers hold. A failed
            // reload reports through the status line and keeps the config that
            // is already running, rather than ending the session (E9).
            if app.take_config_reload_request() {
                reload_config(&mut app, &mut config, &options.config_path);
            }
        }
    }

    // The exit save is forced past the rate limit, and runs even when the loop
    // ended on a fatal draw error, so nothing edited since the last periodic
    // save is lost. Only here is a save failure worth reporting as an error:
    // there is no later retry.
    let save_result = save_if_dirty(&mut app, store, &mut last_save, Instant::now(), true);
    remove_agent_status_files(&app);
    match fatal {
        Some(error) => Err(error),
        // The boundary: `run` reports to `main` in `io::Result`, so the typed
        // save failure is converted here rather than being one all the way down.
        None => save_result.map_err(io::Error::from),
    }
}

/// The terminal's current size as a frame rect, falling back to `last` when the
/// query fails. `ratatui` asks the backend for the same size when it resizes its
/// buffer; asking here too is what lets one `AppLayout` be shared by the
/// renderer and the handlers instead of the renderer computing its own.
fn terminal_frame_area(terminal: &DefaultTerminal, last: Rect) -> Rect {
    match terminal.size() {
        Ok(size) => Rect::new(0, 0, size.width, size.height),
        Err(_) => last,
    }
}

/// Re-read `config.json` and swap it in without restarting (E9).
///
/// Everything the renderer reads comes off the `Config` each frame, so the new
/// palette, commands and project list apply on the next draw; `mouse_capture`
/// is the exception, because it was pushed to the host terminal at startup and
/// only the next start can change it. A reload that fails leaves the running
/// config exactly as it was.
fn reload_config(app: &mut App, config: &mut Config, path: &Path) {
    match config::load_from(path) {
        Ok(reloaded) => {
            let mouse_capture_changed = reloaded.mouse_capture != config.mouse_capture;
            *config = reloaded;
            app.push_status_notice(StatusLevel::Info, format!("reloaded {}", path.display()));
            for warning in config.warnings() {
                app.push_status_notice(StatusLevel::Warning, warning);
            }
            if mouse_capture_changed {
                app.push_status_notice(
                    StatusLevel::Info,
                    "mouse_capture only takes effect on the next start",
                );
            }
        }
        Err(error) => app.set_last_error(error.to_string()),
    }
}

/// Idle backoff for the event-loop poll interval.
///
/// The loop woke 62.5 times a second regardless of activity, and every wake ran
/// the resize check, the status poll, a `waitpid` per running agent child and
/// several full workspace scans. After `IDLE_TICKS_BEFORE_BACKOFF` consecutive
/// ticks with nothing to do, the wait grows to `IDLE_EVENT_POLL_INTERVAL`.
///
/// Input latency does not regress: `event::poll` returns the moment a key or
/// mouse event arrives rather than waiting out its timeout, and the handler
/// immediately calls `record_activity`, so the tick that follows any input is
/// back to `EVENT_POLL_INTERVAL`. The same reset happens for PTY output, agent
/// events and status changes, which do arrive on channels and are therefore
/// noticed up to one interval late — once, at the start of a burst.
#[derive(Debug, Default)]
struct IdleBackoff {
    idle_ticks: u32,
}

impl IdleBackoff {
    fn record_activity(&mut self) {
        self.idle_ticks = 0;
    }

    fn record_idle_tick(&mut self) {
        self.idle_ticks = self.idle_ticks.saturating_add(1);
    }

    fn poll_interval(&self) -> Duration {
        if self.idle_ticks >= IDLE_TICKS_BEFORE_BACKOFF {
            IDLE_EVENT_POLL_INTERVAL
        } else {
            EVENT_POLL_INTERVAL
        }
    }
}

/// Ask the branch watcher for a refresh. Cheap and non-blocking: it hands the
/// workspace list to the probe thread and returns, and a request made while one
/// is still in flight is dropped.
fn request_workspace_git_branches(app: &App, watcher: &mut git::BranchWatcher) {
    let workspaces = app
        .project
        .workspaces
        .iter()
        .map(|workspace| (workspace.id, workspace.cwd.clone()))
        .collect::<Vec<_>>();
    watcher.request(workspaces);
}

fn drain_pty_events(app: &mut App, pty_runtime: &mut PtyRuntime) -> bool {
    let mut changed = false;
    for event in pty_runtime.drain_events() {
        changed = true;
        match event {
            PtyEvent::Scrollback { .. } | PtyEvent::Output { .. } => {}
            PtyEvent::Exited { pty, status } => match pty {
                PtyKey::ChatAgent(chat_id) => {
                    let chat_status = if status.code == 0 {
                        // `mark_chat_status_by_id` decides the seen bit from
                        // the selection; this is only the state (F16).
                        ChatStatus::Done { seen: false }
                    } else {
                        ChatStatus::Failed
                    };
                    let agent = chat_agent_kind(app, chat_id);
                    app.mark_chat_status_by_id(chat_id, chat_status);
                    // The agent process owned this file; left behind, the next
                    // poll would read its last value (typically `running`) and
                    // flip the chat straight back to `Thinking`, permanently.
                    if let Some(path) = mult_agent_status_path(chat_id) {
                        let _ = fs::remove_file(path);
                    }
                    if app.pty_input_target() == Some(pty) {
                        app.end_pty_input();
                    }
                    let exit_message =
                        format!("{} agent exited: {}", agent.display_name(), status.label());
                    pty_runtime.append_pty_system_line(pty, exit_message.as_str());
                }
                PtyKey::Terminal(terminal_id) => {
                    app.set_terminal_restore_on_launch(terminal_id, false);
                    if app.terminal_input_target() == Some(terminal_id) {
                        app.end_pty_input();
                    }
                    let exit_message = format!("PTY exited: {}", status.label());
                    pty_runtime.append_pty_system_line(pty, exit_message.as_str());
                }
            },
            PtyEvent::Error { pty, message, .. } => {
                // A pane-less error belongs to no PTY; writing it into an
                // arbitrary one is how it used to end up in whichever pane the
                // hash order produced — and, for the connect failure, in pane
                // `TerminalId(0)`, which cannot exist. It goes to the status
                // line instead (E2).
                match pty {
                    Some(pty) => {
                        pty_runtime.append_pty_system_line(pty, message.as_str());
                    }
                    None => app.set_last_error(message),
                }
            }
            // Progress from the background connector (B6): informational, so it
            // never masquerades as a failure.
            PtyEvent::Notice { message } => app.push_status_notice(StatusLevel::Info, message),
        }
    }
    // Connection-wide daemon failures (a protocol mismatch, an eviction) never
    // name a pane, so they are held on the runtime rather than queued as an
    // event; the status line is where they become visible (E2).
    if let Some(message) = pty_runtime.take_last_server_error() {
        app.set_last_error(format!("mult-server: {message}"));
        changed = true;
    }
    // Output left queued by the per-frame drain budget still has to be drawn,
    // and picked up on the next tick.
    changed |= pty_runtime.has_deferred_work();
    changed
}

/// Persist the project when a save is due.
///
/// Ordinary saves are rate-limited to `SAVE_INTERVAL`; structural changes (a
/// workspace, chat or terminal added or removed) and the exit save bypass the
/// timer, so nothing a user would notice can wait more than a tick.
fn save_if_dirty(
    app: &mut App,
    store: &mut dyn StateStore,
    last_save: &mut Instant,
    now: Instant,
    force: bool,
) -> Result<(), storage::StateError> {
    if !save_is_due(
        app.is_dirty(),
        app.needs_urgent_save(),
        force,
        now.saturating_duration_since(*last_save),
    ) {
        return Ok(());
    }

    // Stamped before the write so a failing save backs off exactly like a
    // successful one instead of retrying every frame. `dirty` is only cleared
    // on success, so the next due save picks the change up again.
    *last_save = now;
    store.save(&app.project)?;
    app.mark_clean();
    Ok(())
}

fn save_is_due(dirty: bool, urgent: bool, force: bool, since_last_save: Duration) -> bool {
    dirty && (force || urgent || since_last_save >= SAVE_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{self, ProjectState};

    #[test]
    fn a_pane_less_daemon_error_reaches_the_status_line() {
        // E2: this message used to be queued against
        // `PtyKey::Terminal(TerminalId(0))` — terminal ids start at 1, so it
        // rendered into a pane that cannot exist and the user with no
        // `mult-server` saw an inert UI and no explanation.
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::offline_with_pending_events(vec![PtyEvent::Error {
            pty: None,
            code: mult_protocol::RejectCode::Unspecified,
            message: "failed to connect to mult-server: No such file or directory".to_string(),
        }]);

        assert!(drain_pty_events(&mut app, &mut pty_runtime));

        let notice = app
            .current_status_notice()
            .expect("the failure is surfaced");
        assert_eq!(notice.level, StatusLevel::Error);
        assert!(
            notice.message.contains("failed to connect to mult-server"),
            "{}",
            notice.message
        );
    }
    /// A3: the instance token is allocated once and then persisted, so a
    /// restarted client reclaims its own panes instead of taking a new
    /// namespace and abandoning them.
    #[test]
    fn the_instance_token_is_allocated_once_and_marks_the_state_dirty() {
        let mut app = App::two_workspaces();
        app.mark_clean();

        let token = app.ensure_instance_token();

        assert_ne!(token, 0);
        assert!(app.is_dirty(), "a new token has to reach the state file");
        app.mark_clean();
        assert_eq!(app.ensure_instance_token(), token);
        assert!(!app.is_dirty(), "an existing token is not a change");
    }
    #[test]
    fn a_pane_bound_daemon_error_still_goes_to_its_pane() {
        let mut app = App::two_workspaces();
        let terminal = PtyKey::Terminal(model::TerminalId::new(7).unwrap());
        let mut pty_runtime = PtyRuntime::offline_with_pending_events(vec![PtyEvent::Error {
            pty: Some(terminal),
            code: mult_protocol::RejectCode::PaneOperationFailed,
            message: "pane failure".to_string(),
        }]);

        drain_pty_events(&mut app, &mut pty_runtime);

        assert!(app.current_status_notice().is_none());
        assert!(pty_runtime
            .pty_lines(terminal)
            .iter()
            .any(|line| line.contains("pane failure")));
    }
    #[test]
    fn reloading_the_config_swaps_it_in_and_reports_the_swap() {
        let path = unique_temp_config();
        fs::write(&path, r##"{"colorscheme":{"base":"#010203"}}"##).expect("write config");
        let mut app = App::two_workspaces();
        let mut config = Config::default();

        reload_config(&mut app, &mut config, &path);

        assert_eq!(
            config.colors().base,
            crate::config::Rgb {
                red: 1,
                green: 2,
                blue: 3
            }
        );
        let notice = app.current_status_notice().expect("the reload is reported");
        assert_eq!(notice.level, StatusLevel::Info);
        assert!(notice.message.contains(&path.display().to_string()));
        let _ = fs::remove_file(&path);
    }
    #[test]
    fn a_failed_reload_reports_and_keeps_the_running_config() {
        // The E9 requirement that separates a reload from a restart: a config
        // that stops parsing must not be able to end a session that is running
        // fine on the config it already has.
        let path = unique_temp_config();
        fs::write(&path, "{ not json").expect("write config");
        let mut app = App::two_workspaces();
        let mut config = Config {
            pi_agent_command: "pi --keep-me".to_string(),
            ..Config::default()
        };

        reload_config(&mut app, &mut config, &path);

        assert_eq!(config.pi_agent_command, "pi --keep-me");
        let notice = app
            .current_status_notice()
            .expect("the failure is reported");
        assert_eq!(notice.level, StatusLevel::Error);
        assert!(
            notice
                .message
                .starts_with(&format!("config error at {}:", path.display())),
            "{}",
            notice.message
        );
        let _ = fs::remove_file(&path);
    }
    fn unique_temp_config() -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mult-runtime-test-{unique}-{}.json",
            std::process::id()
        ))
    }
    #[test]
    fn idle_backoff_slows_polling_and_input_snaps_it_back() {
        let mut idle = IdleBackoff::default();

        assert_eq!(idle.poll_interval(), EVENT_POLL_INTERVAL);
        for _ in 0..IDLE_TICKS_BEFORE_BACKOFF - 1 {
            idle.record_idle_tick();
            assert_eq!(idle.poll_interval(), EVENT_POLL_INTERVAL);
        }
        idle.record_idle_tick();
        assert_eq!(idle.poll_interval(), IDLE_EVENT_POLL_INTERVAL);

        // Any activity — a key, PTY output, an agent status change — restores
        // the responsive interval for the very next wait, so input latency
        // cannot regress past one already-interrupted poll.
        idle.record_activity();
        assert_eq!(idle.poll_interval(), EVENT_POLL_INTERVAL);
    }
    #[test]
    fn saves_are_rate_limited_except_when_urgent_or_forced() {
        // Nothing to write.
        assert!(!save_is_due(false, false, false, Duration::from_secs(60)));
        assert!(!save_is_due(false, true, true, Duration::from_secs(60)));

        // Streamed chat text waits for the interval.
        assert!(!save_is_due(true, false, false, Duration::ZERO));
        assert!(!save_is_due(
            true,
            false,
            false,
            SAVE_INTERVAL - Duration::from_millis(1)
        ));
        assert!(save_is_due(true, false, false, SAVE_INTERVAL));

        // Structural changes and the exit save do not.
        assert!(save_is_due(true, true, false, Duration::ZERO));
        assert!(save_is_due(true, false, true, Duration::ZERO));
    }
    /// F11: what the loop actually writes, observed without touching a
    /// filesystem or the process environment. Slice 4's rate limit and its
    /// forced exit save are behavioural claims about a *store*, and until the
    /// store was a parameter nothing could check them end to end.
    #[test]
    fn save_if_dirty_honours_the_rate_limit_and_the_forced_exit_save() {
        let mut app = App::two_workspaces();
        let terminal = app.project.workspaces[0].terminals[0].id;
        let mut store = storage::MemoryStateStore::default();
        let start = Instant::now();
        let mut last_save = start;

        // Clean: nothing is written at all, forced or not.
        app.mark_clean();
        save_if_dirty(&mut app, &mut store, &mut last_save, start, true).expect("clean save");
        assert_eq!(store.save_count(), 0);

        // Dirty but not urgent, and inside the interval: still nothing.
        app.set_terminal_restore_on_launch(terminal, true);
        assert!(app.is_dirty());
        assert!(!app.needs_urgent_save());
        save_if_dirty(&mut app, &mut store, &mut last_save, start, false).expect("early save");
        assert_eq!(store.save_count(), 0);
        assert!(app.is_dirty(), "an elided save must not clear the flag");

        // Past the interval: written once, and the flag clears.
        let due = start + SAVE_INTERVAL;
        save_if_dirty(&mut app, &mut store, &mut last_save, due, false).expect("due save");
        assert_eq!(store.save_count(), 1);
        assert!(!app.is_dirty());
        assert_eq!(store.saved(), Some(&app.project));

        // The exit save bypasses the timer, but only when there is something
        // to write.
        app.set_terminal_restore_on_launch(terminal, false);
        save_if_dirty(&mut app, &mut store, &mut last_save, due, true).expect("forced save");
        assert_eq!(store.save_count(), 2);
    }
    /// The other half of F11: a store that answers `load` is what the loop
    /// starts from, so a session can be set up without a state file existing.
    #[test]
    fn a_memory_store_round_trips_a_project() {
        let project = ProjectState::two_workspaces();
        let store = storage::MemoryStateStore::with_state(project.clone());

        let loaded = store.load().expect("load from memory");

        assert_eq!(loaded.state, project);
        assert!(!loaded.first_run);
        assert!(loaded.notice.is_none());
        assert!(
            storage::MemoryStateStore::default()
                .load()
                .expect("load empty")
                .first_run,
            "an empty store is a first run, exactly like a missing file"
        );
    }
}
