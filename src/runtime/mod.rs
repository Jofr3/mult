//! Runtime orchestration for the `mult` client: the event loop and the wiring
//! that drives `App`, the `PtyRuntime`, and the agent backend. `main.rs` keeps
//! only terminal setup/teardown and calls [`run`].
//!
//! Everything the loop *does* per tick lives in a submodule — input dispatch,
//! mouse hit-testing, the key encoder, the clipboard, agent launch and status,
//! session lifecycle, save scheduling. This module owns only the loop itself,
//! the per-tick ordering, config reload, and host-terminal failure policy.

mod agent_command;
mod agent_launch;
mod agent_status;
mod clipboard;
mod input;
mod keymap;
mod mouse;
mod prompt;
mod save;
mod session;
#[cfg(test)]
mod test_support;

use std::{
    io::{self},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use crossterm::event::{self};
use mult_protocol::peer::effective_uid;
use ratatui::{layout::Rect, DefaultTerminal};

use crate::layout::AppLayout;
use crate::{
    app::{App, NoticeLevel, NoticeSource},
    config::{self, Config},
    git,
    model::{self},
    pty::{PtyRuntime, SpawnPolicy},
    storage, ui,
};

use self::agent_launch::{auto_start_selected_chat_agent, drain_agent_events, RuntimeAgentBackend};
use self::agent_status::{drain_mult_agent_status_events, AgentStatusBridge, JournalStatusSource};
use self::clipboard::flush_host_terminal_writes;
use self::input::handle_event;
use self::save::{save_content_if_due, save_if_dirty_with, SaveSchedule};
use self::session::{
    auto_start_selected_terminal, drain_pty_events, register_project_session_identities,
    resize_visible_chat_agent, resize_visible_terminal, restore_persisted_sessions,
};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);

const READY_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(0);

const GIT_BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Run the client event loop.
///
/// `store` is the process-lifetime state lock `main` acquired: it is the single
/// save path, so no code here re-resolves the state path from the environment
/// (B16).
pub fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    mut config: Config,
    shutdown: &AtomicBool,
    store: &storage::StateStore,
) -> io::Result<()> {
    let save = |state: &model::ProjectState| store.save(state);
    // Explicitly, never `Default`: this connects a socket and may fork a
    // daemon (F3).
    let mut pty_runtime =
        PtyRuntime::with_socket_path(mult_protocol::default_socket_path(), SpawnPolicy::Autospawn);
    let mut agent_backend = RuntimeAgentBackend::from_env();
    let mut agent_status_bridge = AgentStatusBridge::<JournalStatusSource>::default();
    let mut save_schedule = SaveSchedule::default();
    let size = terminal.size()?;
    let mut frame_area = Rect::new(0, 0, size.width, size.height);
    // A rejected colour or an unusable project path is a real problem the user
    // cannot see once the alternate screen is open (E2/E6).
    report_config_warnings(&mut app, &config);
    register_project_session_identities(&app, &mut pty_runtime);
    let mut layout = AppLayout::compute(&app, frame_area);
    restore_persisted_sessions(&mut app, &mut pty_runtime, &config, layout);
    refresh_workspace_git_branches(&mut app);
    let mut last_git_branch_refresh = Instant::now();

    // The screen is static unless something changes, so only rebuild a frame
    // when needed instead of every ~16ms tick. The tick still runs so PTY/agent
    // output (delivered over channels, not via event::poll) is drained promptly;
    // it is just the expensive draw that is gated. `needs_redraw` is set by any
    // input event, drained PTY/agent/status change, git-branch refresh, or an
    // auto-start/resize that altered state.
    let mut needs_redraw = true;

    // Host-terminal I/O is the one failure the loop cannot simply report: if
    // `draw`, `poll` or `read` fails for good (the window closed, the ssh
    // session dropped, the pty returned EIO) there is no UI left to show an
    // error in. Such a failure must still leave through the same exit path as a
    // quit, so unsaved state is checkpointed and `TerminalGuard` restores the
    // user's TTY; only the error itself is propagated afterwards. Transient
    // failures (`Interrupted`, `WouldBlock`, `TimedOut`) are retried on the next
    // tick instead of ending the session.
    macro_rules! host_terminal_io {
        ($result:expr, $recovered:expr) => {
            match $result {
                Ok(value) => value,
                Err(error) => match classify_host_terminal_error(error) {
                    HostTerminalFailure::Retry => $recovered,
                    HostTerminalFailure::Fatal(error) => {
                        return finish_after_host_terminal_error_with(&mut app, error, save);
                    }
                },
            }
        };
    }

    while !app.should_quit {
        // One clock reading per tick drives every timer below.
        let now = Instant::now();
        if shutdown.load(Ordering::Relaxed) {
            // Signals must not strand the terminal in raw mode. Make one
            // best-effort checkpoint, then return so the terminal guard can
            // restore the user's TTY even when persistence remains broken.
            save_if_dirty_with(&mut app, true, save);
            return Ok(());
        }
        if now.saturating_duration_since(last_git_branch_refresh) >= GIT_BRANCH_REFRESH_INTERVAL {
            // Only an actual branch change is a reason to rebuild the frame.
            needs_redraw |= refresh_workspace_git_branches(&mut app);
            last_git_branch_refresh = now;
        }
        // A transient notice that has run out of time is a reason to redraw and
        // nothing else; a quiet session never gets here with anything pending.
        needs_redraw |= app.expire_notices(now);
        needs_redraw |= reload_config_if_requested(&mut app, &mut config);
        needs_redraw |= drain_pty_events(&mut app, &mut pty_runtime);
        needs_redraw |= drain_agent_events(&mut app, &mut agent_backend);
        needs_redraw |= drain_mult_agent_status_events(
            &mut app,
            &mut pty_runtime,
            &mut agent_status_bridge,
            now,
        );
        // Resolved here, and only here: after everything that can change what
        // the frame has to hold — an expired or freshly drained notice, a
        // reloaded config, a `ConnectionError` pushed by `drain_pty_events` —
        // and immediately before the surfaces that consume it. Resolving it at
        // the top of the tick instead would size the panes for a status surface
        // one notice out of date, and since the draw below clears `needs_redraw`
        // a quiet session would keep that frame until the next event (F6).
        // Nothing between here and the draw pushes a notice or opens a prompt,
        // so this layout is still the right one when the frame is painted.
        layout = AppLayout::compute(&app, frame_area);

        needs_redraw |= save_content_if_due(&mut app, &mut save_schedule, now, save);
        needs_redraw |= resize_visible_terminal(&mut app, &mut pty_runtime, &config, layout);
        needs_redraw |= resize_visible_chat_agent(&mut app, &mut pty_runtime, &config, layout);
        needs_redraw |= auto_start_selected_terminal(&mut app, &mut pty_runtime, &config, layout);
        needs_redraw |=
            auto_start_selected_chat_agent(&mut app, &mut pty_runtime, &config, store, layout);

        if needs_redraw {
            // A retried draw keeps `needs_redraw` set so the frame is rebuilt on
            // the next tick rather than silently skipped.
            let drawn = host_terminal_io!(
                terminal
                    .draw(|frame| ui::draw(frame, &app, &pty_runtime, &config, layout))
                    .map(|completed| Some(completed.area)),
                None
            );
            if let Some(area) = drawn {
                // A host-terminal resize only becomes visible here: `ratatui`
                // resizes its buffer inside `draw`. Re-resolving keeps the
                // events read below hit-testing against the geometry the user
                // is looking at, which is what the pre-`AppLayout` loop did by
                // handing the handlers the freshly drawn `frame_area`.
                if area != frame_area {
                    frame_area = area;
                    layout = AppLayout::compute(&app, frame_area);
                }
                needs_redraw = false;
            }
        }
        flush_host_terminal_writes(terminal, &mut pty_runtime);

        if host_terminal_io!(event::poll(EVENT_POLL_INTERVAL), false) {
            if let Some(event) = host_terminal_io!(event::read().map(Some), None) {
                handle_event(&mut app, &mut pty_runtime, &config, store, event, layout);
                needs_redraw = true;
                while !app.should_quit
                    && host_terminal_io!(event::poll(READY_EVENT_POLL_INTERVAL), false)
                {
                    let Some(event) = host_terminal_io!(event::read().map(Some), None) else {
                        break;
                    };
                    handle_event(&mut app, &mut pty_runtime, &config, store, event, layout);
                }
                // Everything the user just did is persisted before the loop can
                // observe `should_quit`, so a quit never leaves work behind.
                // This is also the retry for a previously failed save.
                needs_redraw |= save_if_dirty_with(&mut app, true, save);
                save_schedule.record(now);
            }
        }
    }

    Ok(())
}

/// Re-read `config.json` and swap it in place when the user asked for it (E9).
///
/// A failure is reported through the status surface and the old config is kept:
/// a typo in a colorscheme must not end a session that is holding live PTYs.
/// Returns whether the frame needs rebuilding.
fn reload_config_if_requested(app: &mut App, config: &mut Config) -> bool {
    if !app.take_config_reload_request() {
        return false;
    }

    // No `--config` here on purpose: the flag is `main`'s, and re-resolving it
    // would need it threaded through the whole event loop. `$MULT_CONFIG_PATH`
    // and the default path still apply, which is where a reloadable config
    // realistically lives.
    match config::load_or_default(None) {
        Ok(reloaded) => {
            report_config_warnings(app, &reloaded);
            if reloaded == *config {
                app.push_notice(
                    NoticeLevel::Info,
                    NoticeSource::Report,
                    "Config reloaded; nothing changed.",
                );
            } else {
                let mouse_capture_changed = reloaded.mouse_capture != config.mouse_capture;
                *config = reloaded;
                // Everything read per frame (colorscheme, projects, agent
                // commands, auto-start) applies immediately. Mouse capture is
                // a terminal mode `main` sets once around the whole session,
                // and a PTY already running keeps the command it was started
                // with — say so rather than let it look like a no-op.
                let caveat = if mouse_capture_changed {
                    " mouse_capture needs a restart; already-running PTYs keep their command."
                } else {
                    " Already-running PTYs keep the command they were started with."
                };
                app.push_notice(
                    NoticeLevel::Info,
                    NoticeSource::Report,
                    format!("Config reloaded.{caveat}"),
                );
            }
        }
        Err(error) => {
            app.push_notice(
                NoticeLevel::Error,
                NoticeSource::Report,
                format!(
                    "Config reload failed; keeping the previous config: {error} ({})",
                    config::config_path().display()
                ),
            );
        }
    }
    true
}

/// Put the config loader's non-fatal complaints on the status surface (E2).
///
/// They are already complete sentences naming the file, so they are shown
/// verbatim; `main` also prints them to stderr, which is what a user sees
/// before the alternate screen opens and after it closes.
fn report_config_warnings(app: &mut App, config: &Config) {
    for warning in config.warnings() {
        app.push_notice(NoticeLevel::Warning, NoticeSource::Report, warning.clone());
    }
}

/// What the render loop should do about a failed host-terminal operation.
enum HostTerminalFailure {
    /// Transient: skip this step and try again on the next tick.
    Retry,
    /// The host terminal is unusable; end the session through the normal exit
    /// path and report this error.
    Fatal(io::Error),
}

fn classify_host_terminal_error(error: io::Error) -> HostTerminalFailure {
    match error.kind() {
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            HostTerminalFailure::Retry
        }
        // Everything else — EIO from a vanished pty, a broken pipe, a closed
        // window — is permanent. `ErrorKind` has no variant for EIO, so it lands
        // here rather than being matched by name.
        _ => HostTerminalFailure::Fatal(error),
    }
}

/// Exit after an unrecoverable host-terminal failure without losing state.
///
/// The session is over either way; the point is that everything the quit path
/// does still happens. A best-effort forced save runs first (so work since the
/// last checkpoint survives a closed window), and the caller's `TerminalGuard`
/// restores the TTY when this error unwinds out of `run`.
pub(super) fn finish_after_host_terminal_error_with(
    app: &mut App,
    error: io::Error,
    saver: impl FnMut(&model::ProjectState) -> storage::StateResult<()>,
) -> io::Result<()> {
    save_if_dirty_with(app, true, saver);
    Err(error)
}

/// Re-read every workspace's checked-out branch. Returns whether any of them
/// changed — a branch is only visible in the sidebar, so an unchanged probe must
/// not force a redraw the loop otherwise skips (S4).
fn refresh_workspace_git_branches(app: &mut App) -> bool {
    let branches = app
        .project
        .workspaces
        .iter()
        .map(|workspace| {
            let branch = workspace.cwd.as_deref().and_then(git::current_branch);
            (workspace.id, branch)
        })
        .collect::<Vec<_>>();
    app.replace_workspace_git_branches(branches)
}

pub(super) fn ensure_mult_runtime_dir() -> io::Result<PathBuf> {
    let dir = mult_runtime_dir();
    mult_protocol::ensure_private_dir(&dir)?;
    Ok(dir)
}

pub(super) fn mult_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("mult-{}", effective_uid())))
        .join("mult")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::runtime::test_support::*;
    use std::fs;

    /// S4: the git probe runs every two seconds for every workspace, and its
    /// result is only visible in the sidebar. An unchanged branch must not
    /// force the redraw the loop otherwise skips.
    #[test]
    fn an_unchanged_git_branch_is_not_a_redraw_reason() {
        let root = unique_status_path("git-branch").with_extension("repo");
        fs::create_dir_all(root.join(".git")).expect("create fixture repository");
        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")
            .expect("write fixture HEAD");

        let mut app = App::default();
        app.project.workspaces[0].cwd = Some(root.clone());

        assert!(
            refresh_workspace_git_branches(&mut app),
            "the first probe learns the branch"
        );
        assert_eq!(
            app.workspace_git_branch(app.project.workspaces[0].id),
            Some("main")
        );
        assert!(
            !refresh_workspace_git_branches(&mut app),
            "an unchanged branch reports no change"
        );

        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/side\n")
            .expect("switch fixture branch");
        assert!(refresh_workspace_git_branches(&mut app));
        assert_eq!(
            app.workspace_git_branch(app.project.workspaces[0].id),
            Some("side")
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// B11: draw/poll/read failures must not propagate straight out of `run`.
    /// Transient ones are retried, and a permanent one still checkpoints state
    /// on its way out instead of discarding everything since the last save.
    #[test]
    fn host_terminal_failures_are_classified_and_checkpointed_before_exit() {
        assert!(matches!(
            classify_host_terminal_error(io::Error::from(io::ErrorKind::Interrupted)),
            HostTerminalFailure::Retry
        ));
        assert!(matches!(
            classify_host_terminal_error(io::Error::from(io::ErrorKind::WouldBlock)),
            HostTerminalFailure::Retry
        ));
        assert!(matches!(
            classify_host_terminal_error(io::Error::from(io::ErrorKind::BrokenPipe)),
            HostTerminalFailure::Fatal(_)
        ));
        // A host terminal that disappears reports EIO, which has no `ErrorKind`
        // of its own and must still be treated as unrecoverable.
        assert!(matches!(
            classify_host_terminal_error(io::Error::from_raw_os_error(libc::EIO)),
            HostTerminalFailure::Fatal(_)
        ));

        let mut app = App::default();
        app.add_terminal_to_selected_workspace();
        assert!(app.is_dirty());
        let saved = Cell::new(false);

        let error = finish_after_host_terminal_error_with(
            &mut app,
            io::Error::from_raw_os_error(libc::EIO),
            |_| {
                saved.set(true);
                Ok(())
            },
        )
        .expect_err("a fatal host-terminal error is still reported to the caller");

        assert!(
            saved.get(),
            "state since the last save must be checkpointed before the session ends"
        );
        assert!(!app.is_dirty());
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
    }
}
