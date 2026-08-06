//! Runtime orchestration for the `mult` client: the event loop and all the
//! glue that drives `App`, the `PtyRuntime`, and the agent backend. `main.rs`
//! keeps only terminal setup/teardown and calls `runtime::run`.

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use mult::{
    agent::{
        self, AgentBackend, AgentEvent, NoopAgentBackend, ProcessAgentBackend, ProcessAgentCommand,
    },
    app::{
        App, CommandAction, NavItem, NoticeLevel, NoticeSource, Prompt, PromptEdit, SelectionCell,
        TextSelection,
    },
    config::{self, Config},
    git,
    model::{self, AgentKind, ChatStatus, PtyKey, TerminalLaunch},
    pty::{AttachExistingResult, PtyDimensions, PtyEvent, PtyRuntime, PtySpawn, SpawnPolicy},
    storage, ui,
};
use mult_protocol::{
    peer::effective_uid, shell::quote_argument, AgentGeneration as WireAgentGeneration,
    AgentKind as WireAgentKind, AgentSessionMetadata, AgentStatus, AgentStatusQuery,
    AgentStatusRecord, AGENT_STATUS_SCHEMA_VERSION,
};
use ratatui::{layout::Rect, DefaultTerminal};
use serde::Deserialize;

const AGENT_CMD_ENV: &str = "MULT_AGENT_CMD";
const MULT_AGENT_STATUS_PATH_ENV: &str = "MULT_AGENT_STATUS_PATH";
const MULT_AGENT_CHAT_ID_ENV: &str = "MULT_AGENT_CHAT_ID";
const MULT_AGENT_STATUS_VERSION_ENV: &str = "MULT_AGENT_STATUS_VERSION";
const MULT_AGENT_NAMESPACE_ENV: &str = "MULT_AGENT_NAMESPACE";
const MULT_AGENT_SESSION_TOKEN_ENV: &str = "MULT_AGENT_SESSION_TOKEN";
const MULT_AGENT_KIND_ENV: &str = "MULT_AGENT_KIND";
const MULT_AGENT_GENERATION_ENV: &str = "MULT_AGENT_GENERATION";
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const READY_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(0);
const GIT_BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// How often the per-chat agent status journals are read (S3/B3).
const AGENT_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Minimum spacing between two rate-limited state saves (B9).
const MIN_CONTENT_SAVE_INTERVAL: Duration = Duration::from_secs(1);
const MOUSE_SCROLL_ROWS: usize = 3;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MultAgentStatusRecord {
    version: u16,
    namespace: String,
    session_token: String,
    chat_id: String,
    agent_kind: String,
    generation: String,
    status: String,
}

/// Where the client reads agent status transitions from.
///
/// The agent status bridge is the only per-frame external dependency the client
/// polls that is neither the daemon nor the host terminal, and it was concrete
/// and file-backed, so every test of it had to write real files with real modes
/// into a real temporary directory (F10). The seam splits the two things that
/// were tangled: *what the records mean*, which is pure logic worth testing on
/// a double, and *how a journal file is safely read*, which is a security
/// boundary and must keep being tested against the filesystem.
trait AgentStatusSource {
    /// Records appended for this chat's current agent session since the last
    /// call, oldest first. A source that cannot be read yields nothing: the
    /// daemon remains authoritative, so a missing or malformed journal is not
    /// an error the caller can act on.
    fn poll(
        &mut self,
        chat: model::ChatId,
        identity: model::SessionIdentity,
        generation: model::AgentGeneration,
    ) -> Vec<MultAgentStatusRecord>;

    /// Drops per-chat read state for chats that no longer have a live agent
    /// session. A chat that stopped, or was deleted, keeps no cursor: the
    /// journal it named is gone, and a later chat must never inherit a stale
    /// read offset.
    fn retain(&mut self, live: &[model::ChatId]);
}

/// The agent status bridge: a polling clock plus whichever source it reads.
///
/// The clock exists to keep an idle session cheap (S3/B3): a status dot
/// updating within a quarter second is indistinguishable from instant, whereas
/// 60 Hz `open` + `fstat` + `seek` + `read` + `close` per chat was not.
struct AgentStatusBridge<S> {
    source: S,
    last_poll: Option<Instant>,
}

impl<S: Default> Default for AgentStatusBridge<S> {
    fn default() -> Self {
        Self {
            source: S::default(),
            last_poll: None,
        }
    }
}

impl<S> AgentStatusBridge<S> {
    /// Whether the journals are due to be polled at `now`.
    fn is_due(&self, now: Instant) -> bool {
        self.last_poll
            .is_none_or(|last| now.saturating_duration_since(last) >= AGENT_STATUS_POLL_INTERVAL)
    }
}

/// The production [`AgentStatusSource`]: append-only JSONL journals under the
/// private runtime directory, read without following symlinks.
///
/// The journal path is derived from a namespace, a session token and a
/// generation — four allocations to format — so it is built once per agent
/// session and cached rather than rebuilt on every tick.
#[derive(Default)]
struct JournalStatusSource {
    journals: HashMap<model::ChatId, AgentStatusJournal>,
}

struct AgentStatusJournal {
    /// The identity/generation the cached `path` was built from. A restarted
    /// agent gets a new generation, which invalidates the entry.
    identity: model::SessionIdentity,
    generation: model::AgentGeneration,
    path: PathBuf,
    cursor: AgentStatusCursor,
}

#[derive(Default)]
struct AgentStatusCursor {
    device: u64,
    inode: u64,
    offset: u64,
}

impl JournalStatusSource {
    /// The cached journal for `chat`, rebuilding the path only when the agent
    /// session behind it changed.
    fn journal_for(
        &mut self,
        chat: model::ChatId,
        identity: model::SessionIdentity,
        generation: model::AgentGeneration,
    ) -> &mut AgentStatusJournal {
        let stale = self
            .journals
            .get(&chat)
            .is_none_or(|journal| journal.identity != identity || journal.generation != generation);
        if stale {
            self.journals.insert(
                chat,
                AgentStatusJournal {
                    identity,
                    generation,
                    path: mult_agent_status_path(identity, generation),
                    cursor: AgentStatusCursor::default(),
                },
            );
        }
        self.journals
            .get_mut(&chat)
            .expect("a journal for this chat was just ensured")
    }
}

impl AgentStatusSource for JournalStatusSource {
    fn poll(
        &mut self,
        chat: model::ChatId,
        identity: model::SessionIdentity,
        generation: model::AgentGeneration,
    ) -> Vec<MultAgentStatusRecord> {
        let journal = self.journal_for(chat, identity, generation);
        let AgentStatusJournal { path, cursor, .. } = journal;
        let Ok(records) = read_mult_agent_status_records(path, cursor) else {
            return Vec::new();
        };
        // The cursor advances past every record handed over: the caller cannot
        // reject one back into the journal, and re-reading a consumed record
        // would replay a status transition the daemon already resolved.
        if let Some((_, last_offset)) = records.last() {
            cursor.offset = *last_offset;
        }
        records.into_iter().map(|(record, _)| record).collect()
    }

    fn retain(&mut self, live: &[model::ChatId]) {
        self.journals.retain(|chat, _| live.contains(chat));
    }
}

const MULT_STATUS_EXTENSION_SOURCE: &str = include_str!("../extensions/mult-status.ts");
const MULT_CLAUDE_STATUS_SCRIPT_SOURCE: &str = include_str!("../extensions/mult-claude-status.sh");

enum RuntimeAgentBackend {
    Noop(NoopAgentBackend),
    Process(ProcessAgentBackend),
}

impl RuntimeAgentBackend {
    fn from_env() -> Self {
        std::env::var(AGENT_CMD_ENV)
            .ok()
            .and_then(|raw| parse_process_agent_command(&raw))
            .map(ProcessAgentBackend::new)
            .map(Self::Process)
            .unwrap_or_else(|| Self::Noop(NoopAgentBackend))
    }
}

impl AgentBackend for RuntimeAgentBackend {
    fn send_prompt(&mut self, prompt: agent::AgentPrompt) -> io::Result<()> {
        match self {
            Self::Noop(backend) => backend.send_prompt(prompt),
            Self::Process(backend) => backend.send_prompt(prompt),
        }
    }

    fn drain_events(&mut self) -> Vec<AgentEvent> {
        match self {
            Self::Noop(backend) => backend.drain_events(),
            Self::Process(backend) => backend.drain_events(),
        }
    }
}

fn parse_process_agent_command(raw: &str) -> Option<ProcessAgentCommand> {
    let mut parts = split_process_agent_command(raw).ok()?.into_iter();
    let program = parts.next()?;
    if program.is_empty() {
        return None;
    }

    Some(ProcessAgentCommand::with_args(program, parts))
}

fn split_process_agent_command(raw: &str) -> Result<Vec<String>, &'static str> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut escaping = false;
    let mut in_token = false;

    for ch in raw.chars() {
        if escaping {
            current.push(ch);
            escaping = false;
            in_token = true;
            continue;
        }

        match quote {
            Quote::None => match ch {
                '\\' => {
                    escaping = true;
                    in_token = true;
                }
                '\'' => {
                    quote = Quote::Single;
                    in_token = true;
                }
                '"' => {
                    quote = Quote::Double;
                    in_token = true;
                }
                ch if ch.is_whitespace() => {
                    if in_token {
                        args.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                }
                _ => {
                    current.push(ch);
                    in_token = true;
                }
            },
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => {
                    escaping = true;
                    in_token = true;
                }
                _ => current.push(ch),
            },
        }
    }

    if escaping {
        current.push('\\');
    }
    if quote != Quote::None {
        return Err("unterminated quote");
    }
    if in_token {
        args.push(current);
    }

    Ok(args)
}

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
    restore_persisted_sessions(&mut app, &mut pty_runtime, &config, frame_area);
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
        needs_redraw |= save_content_if_due(&mut app, &mut save_schedule, now, save);
        needs_redraw |= resize_visible_terminal(&mut app, &mut pty_runtime, &config, frame_area);
        needs_redraw |= resize_visible_chat_agent(&mut app, &mut pty_runtime, &config, frame_area);
        needs_redraw |=
            auto_start_selected_terminal(&mut app, &mut pty_runtime, &config, frame_area);
        needs_redraw |=
            auto_start_selected_chat_agent(&mut app, &mut pty_runtime, &config, store, frame_area);

        if needs_redraw {
            // A retried draw keeps `needs_redraw` set so the frame is rebuilt on
            // the next tick rather than silently skipped.
            let drawn = host_terminal_io!(
                terminal
                    .draw(|frame| ui::draw(frame, &app, &pty_runtime, &config))
                    .map(|completed| Some(completed.area)),
                None
            );
            if let Some(area) = drawn {
                frame_area = area;
                needs_redraw = false;
            }
        }
        flush_host_terminal_writes(terminal, &mut pty_runtime);

        if host_terminal_io!(event::poll(EVENT_POLL_INTERVAL), false) {
            if let Some(event) = host_terminal_io!(event::read().map(Some), None) {
                handle_event(
                    &mut app,
                    &mut pty_runtime,
                    &config,
                    store,
                    event,
                    frame_area,
                );
                needs_redraw = true;
                while !app.should_quit
                    && host_terminal_io!(event::poll(READY_EVENT_POLL_INTERVAL), false)
                {
                    let Some(event) = host_terminal_io!(event::read().map(Some), None) else {
                        break;
                    };
                    handle_event(
                        &mut app,
                        &mut pty_runtime,
                        &config,
                        store,
                        event,
                        frame_area,
                    );
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
fn finish_after_host_terminal_error_with(
    app: &mut App,
    error: io::Error,
    saver: impl FnMut(&model::ProjectState) -> storage::StateResult<()>,
) -> io::Result<()> {
    save_if_dirty_with(app, true, saver);
    Err(error)
}

fn register_project_session_identities(app: &App, pty_runtime: &mut PtyRuntime) {
    for workspace in &app.project.workspaces {
        for chat in &workspace.chats {
            let key = PtyKey::ChatAgent(chat.id);
            if let Some(identity) = app.project.session_identity(key) {
                let _ = pty_runtime.register_session_identity(key, identity);
            }
        }
        for terminal in &workspace.terminals {
            let key = PtyKey::Terminal(terminal.id);
            if let Some(identity) = app.project.session_identity(key) {
                let _ = pty_runtime.register_session_identity(key, identity);
            }
        }
    }
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

fn restore_persisted_sessions(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    register_project_session_identities(app, pty_runtime);
    let terminals = app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| {
            // Persisted *intent*: the terminals the user meant to have
            // running. Whether a pane is actually live is the daemon's answer,
            // which the `attach_existing` below asks for (F16). A `Command`
            // whose pane is gone is still never re-executed (C1).
            workspace.terminals.iter().filter_map(|terminal| {
                terminal.restore_on_launch.then_some((
                    workspace.id,
                    terminal.id,
                    terminal.name.clone(),
                    matches!(terminal.launch, TerminalLaunch::Command(_)),
                ))
            })
        })
        .collect::<Vec<_>>();

    for (workspace, terminal, name, is_command) in terminals {
        let key = PtyKey::Terminal(terminal);
        let size = terminal_dimensions(app, frame_area);
        match pty_runtime.attach_existing(key, size) {
            Ok(AttachExistingResult::Attached) => app.record_terminal_started(terminal),
            Ok(AttachExistingResult::Missing) => {
                app.record_terminal_stopped(terminal);
                if is_command {
                    app.mark_terminal_recoverable(terminal);
                    pty_runtime.append_terminal_system_line(
                        key,
                        format!(
                            "command terminal `{name}` was not relaunched because its daemon session is unavailable; type or use Start selected PTY to start it deliberately"
                        ),
                    );
                } else {
                    // Preserve existing shell restoration behavior. The strict
                    // no-relaunch rule applies to configured command terminals.
                    start_terminal(app, pty_runtime, config, frame_area, workspace, terminal);
                }
            }
            Err(error) if is_command => {
                app.record_terminal_stopped(terminal);
                app.mark_terminal_recoverable(terminal);
                pty_runtime.append_terminal_system_line(
                    key,
                    format!("failed to restore terminal `{name}` without relaunching it: {error}"),
                );
            }
            Err(_) => {
                app.record_terminal_stopped(terminal);
                start_terminal(app, pty_runtime, config, frame_area, workspace, terminal);
            }
        }
    }

    let chats = app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.chats.iter().filter_map(|chat| {
                app.project
                    .active_agent_generation(chat.id)
                    .map(|generation| (workspace.id, chat.id, chat.agent, generation))
            })
        })
        .collect::<Vec<_>>();

    for (_workspace, chat, agent, generation) in chats {
        let key = PtyKey::ChatAgent(chat);
        let metadata = agent_session_metadata(chat, agent, generation);
        if let Err(error) = pty_runtime.register_agent_session(key, metadata) {
            app.mark_chat_status_by_id(chat, ChatStatus::Failed);
            pty_runtime.append_terminal_system_line(
                key,
                format!("failed to restore agent generation metadata: {error}"),
            );
            continue;
        }
        let size = chat_agent_dimensions(app, frame_area);
        match pty_runtime.attach_existing(key, size) {
            Ok(AttachExistingResult::Attached) => {
                reconcile_agent_status(app, pty_runtime, chat, agent, generation);
            }
            Ok(AttachExistingResult::Missing) => {
                let recovered_final =
                    reconcile_agent_status(app, pty_runtime, chat, agent, generation);
                if !recovered_final {
                    app.mark_chat_status_by_id(chat, ChatStatus::Failed);
                }
                app.clear_agent_generation(chat, generation);
                pty_runtime.append_terminal_system_line(
                    key,
                    "agent session is unavailable; it was not relaunched during restoration",
                );
            }
            Err(error) => {
                app.mark_chat_status_by_id(chat, ChatStatus::Failed);
                pty_runtime.append_terminal_system_line(
                    key,
                    format!("failed to restore agent without relaunching it: {error}"),
                );
            }
        }
    }
}

fn handle_event(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    event: Event,
    frame_area: Rect,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, pty_runtime, config, store, key, frame_area);
        }
        Event::Mouse(mouse) => handle_mouse(app, pty_runtime, config, mouse, frame_area),
        Event::Paste(text) => handle_paste(app, pty_runtime, config, store, text, frame_area),
        _ => {}
    }
}

fn handle_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    frame_area: Rect,
) {
    if is_quit_key(key) {
        app.quit();
        return;
    }

    // The overlay is modal: while it is up it owns the keyboard, so no key can
    // reach a PTY behind it. Anything that is not a shortcut closes it.
    if app.is_help_visible() {
        handle_help_overlay_key(app, key);
        return;
    }

    match &app.prompt {
        Some(Prompt::OpenWorkspace(_)) => handle_open_workspace_key(app, config, key),
        Some(Prompt::NewTerminalCommand(_)) => handle_terminal_command_key(app, key),
        Some(Prompt::CommandPalette(_)) => {
            handle_command_palette_key(app, pty_runtime, config, store, key, frame_area);
        }
        Some(Prompt::Search(_)) => handle_search_key(app, key),
        Some(Prompt::ConfirmDelete(_)) => handle_delete_confirmation_key(app, pty_runtime, key),
        None => handle_unprompted_key(app, pty_runtime, config, store, key, frame_area),
    }
}

fn handle_mouse(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    mouse: MouseEvent,
    frame_area: Rect,
) {
    if app.is_prompt_active() {
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            begin_text_selection_at_mouse(app, frame_area, mouse);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            update_text_selection_at_mouse(app, frame_area, mouse);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            finish_text_selection_at_mouse(app, pty_runtime, config, frame_area, mouse);
        }
        MouseEventKind::ScrollUp => {
            scroll_output_at_mouse(app, pty_runtime, frame_area, mouse, ScrollDirection::Up);
        }
        MouseEventKind::ScrollDown => {
            scroll_output_at_mouse(app, pty_runtime, frame_area, mouse, ScrollDirection::Down);
        }
        _ => {}
    }
}

fn handle_paste(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    text: String,
    frame_area: Rect,
) {
    if app.is_prompt_active() {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            app.push_prompt_char(ch);
        }
        return;
    }

    let Some(terminal_id) =
        start_selected_pty_if_needed(app, pty_runtime, config, store, frame_area)
    else {
        return;
    };

    match pty_runtime.send_paste(terminal_id, &text) {
        Ok(true) => {}
        Ok(false) => {
            pty_runtime.append_terminal_system_line(terminal_id, "PTY is not running");
        }
        Err(error) => {
            pty_runtime
                .append_terminal_system_line(terminal_id, format!("failed to paste: {error}"));
        }
    }
}

fn begin_text_selection_at_mouse(app: &mut App, frame_area: Rect, mouse: MouseEvent) -> bool {
    let Some((terminal, area)) = selected_output_area(app, frame_area) else {
        app.clear_text_selection();
        return false;
    };
    if !rect_contains(area, mouse.column, mouse.row) {
        app.clear_text_selection();
        return false;
    }
    let Some(cell) = mouse_cell_in_area(area, mouse.column, mouse.row) else {
        return false;
    };
    app.begin_text_selection(terminal, cell);
    true
}

fn update_text_selection_at_mouse(app: &mut App, frame_area: Rect, mouse: MouseEvent) -> bool {
    let Some((terminal, cell)) = active_selection_cell_at_mouse(app, frame_area, mouse) else {
        return false;
    };
    app.update_text_selection(terminal, cell)
}

fn finish_text_selection_at_mouse(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
    mouse: MouseEvent,
) -> bool {
    let Some((terminal, cell)) = active_selection_cell_at_mouse(app, frame_area, mouse) else {
        return false;
    };
    let Some(selection) = app.end_text_selection(terminal, cell) else {
        return false;
    };
    if selection.anchor == selection.focus {
        app.clear_text_selection();
        return false;
    }
    copy_text_selection_to_clipboard(pty_runtime, config, selection);
    true
}

fn copy_current_text_selection(app: &App, pty_runtime: &mut PtyRuntime, config: &Config) -> bool {
    let Some(selection) = app.text_selection else {
        return false;
    };
    copy_text_selection_to_clipboard(pty_runtime, config, selection)
}

fn copy_text_selection_to_clipboard(
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

fn active_selection_cell_at_mouse(
    app: &App,
    frame_area: Rect,
    mouse: MouseEvent,
) -> Option<(PtyKey, SelectionCell)> {
    let selection = app.text_selection?;
    let (terminal, area) = selected_output_area(app, frame_area)?;
    if terminal != selection.terminal {
        return None;
    }
    mouse_cell_in_area(area, mouse.column, mouse.row).map(|cell| (terminal, cell))
}

fn selected_output_area(app: &App, frame_area: Rect) -> Option<(PtyKey, Rect)> {
    if let Some((terminal, area)) = ui::selected_terminal_output_area(app, frame_area) {
        return Some((PtyKey::Terminal(terminal), area));
    }
    ui::selected_chat_agent_output_area(app, frame_area)
        .map(|(chat, area)| (PtyKey::ChatAgent(chat), area))
}

fn mouse_cell_in_area(area: Rect, column: u16, row: u16) -> Option<SelectionCell> {
    if area.is_empty() {
        return None;
    }
    Some(SelectionCell {
        row: i32::from(
            row.saturating_sub(area.y)
                .min(area.height.saturating_sub(1)),
        ),
        col: column
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(1)),
    })
}

fn selected_text(pty_runtime: &PtyRuntime, selection: TextSelection) -> Option<String> {
    let parser = pty_runtime.parser(selection.terminal)?;
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return None;
    }

    let range = selection.normalized_range();
    let visible_last_row = i32::from(rows.saturating_sub(1));
    if range.end.row < 0 || range.start.row > visible_last_row {
        return None;
    }

    let start_row = range.start.row.max(0);
    let end_row = range.end.row.min(visible_last_row);
    let start_col = if start_row == range.start.row {
        range.start.col.min(cols.saturating_sub(1))
    } else {
        0
    };
    let end_col = if end_row == range.end.row {
        range.end.col.min(cols.saturating_sub(1))
    } else {
        cols.saturating_sub(1)
    };
    let start_row = u16::try_from(start_row).unwrap_or(0);
    let end_row = u16::try_from(end_row).unwrap_or(rows.saturating_sub(1));
    let end_col_exclusive = end_col.saturating_add(1).min(cols);
    if start_row == end_row && start_col >= end_col_exclusive {
        return None;
    }

    let text = screen.contents_between(start_row, start_col, end_row, end_col_exclusive);
    (!text.is_empty()).then_some(text)
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
fn flush_host_terminal_writes(terminal: &mut DefaultTerminal, pty_runtime: &mut PtyRuntime) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollDirection {
    Up,
    Down,
}

fn scroll_output_at_mouse(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    frame_area: Rect,
    mouse: MouseEvent,
    direction: ScrollDirection,
) -> bool {
    let Some((terminal, area)) = output_terminal_at(app, frame_area, mouse.column, mouse.row)
    else {
        return false;
    };

    // A program that has grabbed the mouse (Claude Code, nvim, less, ...)
    // scrolls its own view. Our local scrollback holds nothing for it — the
    // alternate screen keeps none — so hand the wheel notch to the program
    // instead of swallowing it into a buffer that can never move.
    if pty_runtime.terminal_reports_mouse(terminal) {
        let Some(cell) = mouse_cell_in_area(area, mouse.column, mouse.row) else {
            return false;
        };
        let col = cell.col.saturating_add(1);
        let row = u16::try_from(cell.row).unwrap_or(0).saturating_add(1);
        return pty_runtime.forward_wheel(terminal, direction == ScrollDirection::Up, col, row);
    }

    match direction {
        ScrollDirection::Up => {
            scroll_terminal_output_up(app, pty_runtime, terminal, MOUSE_SCROLL_ROWS)
        }
        ScrollDirection::Down => {
            scroll_terminal_output_down(app, pty_runtime, terminal, MOUSE_SCROLL_ROWS)
        }
    }
}

fn output_terminal_at(
    app: &App,
    frame_area: Rect,
    column: u16,
    row: u16,
) -> Option<(PtyKey, Rect)> {
    selected_output_area(app, frame_area).filter(|(_, area)| rect_contains(*area, column, row))
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn handle_unprompted_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    frame_area: Rect,
) {
    if opens_help(app, key) {
        app.show_help();
        return;
    }
    if handle_control_key(app, pty_runtime, config, store, key, frame_area) {
        return;
    }

    handle_selected_pty_input_key(app, pty_runtime, config, store, key, frame_area);
}

/// `F1` always opens the overlay; `?` only when no pane would have received it.
///
/// A selected chat or terminal takes every ordinary key — that is how a PTY is
/// started and typed at, there is no input mode to leave — so a global `?`
/// would steal a character from every shell, pager and editor running in a
/// pane. `F1` is safe to take unconditionally: nothing in `mult` sent it
/// anywhere useful before, and it is the one key a full-screen program is
/// unlikely to need. `Ctrl+p` → "Show keybindings" reaches the overlay from
/// anywhere.
fn opens_help(app: &App, key: KeyEvent) -> bool {
    if matches!(key.code, KeyCode::F(1)) {
        return true;
    }
    matches!(key.code, KeyCode::Char('?'))
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && app.help_key_opens_help()
}

/// Any key closes the overlay. It carries no state of its own, so there is
/// nothing to navigate and nothing a stray keystroke can damage — and a user
/// who cannot find the dismissal key is stuck in a modal screen.
fn handle_help_overlay_key(app: &mut App, _key: KeyEvent) {
    app.hide_help();
}

fn handle_control_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    frame_area: Rect,
) -> bool {
    if is_shifted_control_char(key, 'c') {
        copy_current_text_selection(app, pty_runtime, config);
        return true;
    }

    if is_control_down_key(key) {
        app.select_next();
        return true;
    }
    if is_control_up_key(key) {
        app.select_previous();
        return true;
    }
    if is_unshifted_control_char(key, 'q') {
        app.begin_delete_selected();
        return true;
    }
    if is_unshifted_control_char(key, 'p') {
        app.begin_command_palette();
        return true;
    }
    if is_unshifted_control_char(key, 's') && app.begin_search() {
        return true;
    }
    if is_unshifted_control_char(key, 'a') {
        add_agent_to_selected_workspace(app, pty_runtime, config, store, frame_area, AgentKind::Pi);
        return true;
    }
    if is_unshifted_control_char(key, 'x') {
        add_agent_to_selected_workspace(
            app,
            pty_runtime,
            config,
            store,
            frame_area,
            AgentKind::ClaudeCode,
        );
        return true;
    }
    if is_unshifted_control_char(key, 't') {
        app.add_terminal_to_selected_workspace();
        return true;
    }
    if is_unshifted_control_char(key, 'f') {
        app.begin_open_workspace(&config.projects);
        return true;
    }
    // Only consumed when there is something to dismiss, so `Ctrl+n` still
    // reaches a PTY on a quiet session.
    if is_unshifted_control_char(key, 'n') && app.dismiss_notices() {
        return true;
    }

    false
}

fn is_quit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc) && is_control_key(key)
}

fn is_control_down_key(key: KeyEvent) -> bool {
    is_unshifted_control_char(key, 'j')
        || (matches!(key.code, KeyCode::Enter) && is_control_key(key))
}

fn is_control_up_key(key: KeyEvent) -> bool {
    is_unshifted_control_char(key, 'k')
}

fn is_unshifted_control_char(key: KeyEvent, target: char) -> bool {
    let KeyCode::Char(ch) = key.code else {
        return false;
    };

    is_control_key(key)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && ch == target.to_ascii_lowercase()
}

fn is_shifted_control_char(key: KeyEvent, target: char) -> bool {
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

fn scroll_terminal_output_up(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    terminal: PtyKey,
    rows: usize,
) -> bool {
    let before = terminal_scrollback(pty_runtime, terminal);
    let changed = pty_runtime.scroll_up(terminal, rows).unwrap_or(false);
    sync_text_selection_with_scrollback(app, pty_runtime, terminal, before, changed);
    changed
}

fn scroll_terminal_output_down(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    terminal: PtyKey,
    rows: usize,
) -> bool {
    let before = terminal_scrollback(pty_runtime, terminal);
    let changed = pty_runtime.scroll_down(terminal, rows).unwrap_or(false);
    sync_text_selection_with_scrollback(app, pty_runtime, terminal, before, changed);
    changed
}

fn terminal_scrollback(pty_runtime: &PtyRuntime, terminal: PtyKey) -> usize {
    pty_runtime
        .parser(terminal)
        .map(|parser| parser.screen().scrollback())
        .unwrap_or_default()
}

fn sync_text_selection_with_scrollback(
    app: &mut App,
    pty_runtime: &PtyRuntime,
    terminal: PtyKey,
    before: usize,
    changed: bool,
) {
    if !changed {
        return;
    }
    let after = terminal_scrollback(pty_runtime, terminal);
    let delta = (after as i64 - before as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    app.shift_text_selection_rows(terminal, delta);
}

fn add_agent_to_selected_workspace(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    frame_area: Rect,
    agent: AgentKind,
) {
    if let Some((workspace, chat)) = app.add_chat_to_selected_workspace_and_return(agent) {
        start_or_focus_chat_agent(
            app,
            pty_runtime,
            config,
            store,
            frame_area,
            ChatAgentLaunch {
                workspace_id: workspace,
                chat_id: chat,
                focus_after_start: true,
            },
        );
    }
}

fn confirm_pending_delete(app: &mut App, pty_runtime: &mut PtyRuntime) {
    let status_file = app.pending_delete_pty().and_then(|key| match key {
        PtyKey::ChatAgent(chat) => Some(mult_agent_status_path(
            app.project.session_identity(key)?,
            app.project.active_agent_generation(chat)?,
        )),
        PtyKey::Terminal(_) => None,
    });
    let removed = confirm_pending_delete_with(app, |_, terminal| {
        pty_runtime
            .stop(terminal)
            .map(|_| ())
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
    });
    for terminal in &removed {
        pty_runtime.remove_terminal(*terminal);
    }
    if !removed.is_empty() {
        if let Some(path) = status_file {
            let _ = fs::remove_file(path);
        }
    }
}

fn confirm_pending_delete_with(
    app: &mut App,
    mut stop: impl FnMut(&App, PtyKey) -> Result<(), Box<dyn std::error::Error>>,
) -> Vec<PtyKey> {
    if let Some(terminal) = app.pending_delete_pty() {
        // The target is still present here. Durable state is mutated only after
        // the daemon accepted the stop request (or no attachment existed).
        if let Err(error) = stop(app, terminal) {
            app.set_delete_error(format!("failed to stop PTY; item was not deleted: {error}"));
            return Vec::new();
        }
    }

    app.confirm_delete()
}

fn handle_selected_pty_input_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    frame_area: Rect,
) {
    // Emptiness does not depend on cursor-key mode, so this also avoids starting
    // a PTY for keys that map to nothing (e.g. shortcuts handled elsewhere).
    if key_to_pty_bytes_in_mode(key, false).is_empty() {
        return;
    }

    let Some(terminal_id) =
        start_selected_pty_if_needed(app, pty_runtime, config, store, frame_area)
    else {
        return;
    };

    // Honour the application cursor-key mode (DECCKM) the PTY program requested,
    // so arrows reach full-screen apps in the SS3 form they expect.
    let application_cursor = pty_runtime
        .parser(terminal_id)
        .is_some_and(|parser| parser.screen().application_cursor());
    let bytes = key_to_pty_bytes_in_mode(key, application_cursor);

    match pty_runtime.send_input(terminal_id, &bytes) {
        Ok(true) => {}
        Ok(false) => {
            pty_runtime.append_terminal_system_line(terminal_id, "PTY is not running");
        }
        Err(error) => {
            pty_runtime
                .append_terminal_system_line(terminal_id, format!("failed to send input: {error}"));
        }
    }
}

fn start_selected_pty_if_needed(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    frame_area: Rect,
) -> Option<PtyKey> {
    match app.selected_item()? {
        NavItem::Chat { workspace, chat } => {
            let terminal = PtyKey::ChatAgent(chat);
            if pty_runtime.is_running(terminal) {
                app.begin_chat_agent_input();
            } else {
                start_or_focus_chat_agent(
                    app,
                    pty_runtime,
                    config,
                    store,
                    frame_area,
                    ChatAgentLaunch {
                        workspace_id: workspace,
                        chat_id: chat,
                        focus_after_start: true,
                    },
                );
            }
            pty_runtime.is_running(terminal).then_some(terminal)
        }
        NavItem::Terminal {
            workspace,
            terminal,
        } => {
            let key = PtyKey::Terminal(terminal);
            if !pty_runtime.is_running(key) {
                start_terminal(app, pty_runtime, config, frame_area, workspace, terminal);
            }
            if pty_runtime.is_running(key) {
                app.begin_terminal_input();
                Some(key)
            } else {
                None
            }
        }
    }
}

/// What a key means inside a prompt, independent of which prompt it is.
///
/// The four prompt handlers used to repeat the same Esc/Enter/Backspace/Char
/// skeleton with slightly different holes in it (F13). They now share this
/// classifier and differ only in what "submit" and "move" mean for them, so a
/// prompt cannot silently be missing an editing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKey {
    Cancel,
    Submit,
    /// Move the list selection, where the prompt has a list.
    Previous,
    Next,
    Edit(PromptEdit),
    Ignored,
}

/// `Ctrl+k` is deliberately **not** readline's kill-to-end-of-line here.
///
/// It is `mult`'s global "previous item" (the pair of `Ctrl+j`), and two of the
/// five prompts show a list it moves through. Rebinding it to a kill inside the
/// other prompts would make one key mean two things depending on which prompt
/// happened to be open, which is worse than lacking a kill: `Ctrl+u` (delete to
/// start) and `Ctrl+w` (delete the previous word) cover the same ground, and
/// `Delete` removes the character after the cursor. So `Ctrl+k` means
/// "previous" in every prompt, and does nothing in the prompts with no list.
fn classify_prompt_key(key: KeyEvent) -> PromptKey {
    match key.code {
        KeyCode::Esc => PromptKey::Cancel,
        KeyCode::Enter => PromptKey::Submit,
        KeyCode::Up => PromptKey::Previous,
        KeyCode::Down => PromptKey::Next,
        KeyCode::Left => PromptKey::Edit(PromptEdit::MoveLeft),
        KeyCode::Right => PromptKey::Edit(PromptEdit::MoveRight),
        KeyCode::Home => PromptKey::Edit(PromptEdit::MoveHome),
        KeyCode::End => PromptKey::Edit(PromptEdit::MoveEnd),
        KeyCode::Backspace => PromptKey::Edit(PromptEdit::Backspace),
        KeyCode::Delete => PromptKey::Edit(PromptEdit::DeleteForward),
        _ if is_unshifted_control_char(key, 'c') => PromptKey::Cancel,
        _ if is_unshifted_control_char(key, 'k') => PromptKey::Previous,
        _ if is_unshifted_control_char(key, 'j') => PromptKey::Next,
        _ if is_unshifted_control_char(key, 'a') => PromptKey::Edit(PromptEdit::MoveHome),
        _ if is_unshifted_control_char(key, 'e') => PromptKey::Edit(PromptEdit::MoveEnd),
        _ if is_unshifted_control_char(key, 'w') => PromptKey::Edit(PromptEdit::DeleteWordBefore),
        _ if is_unshifted_control_char(key, 'u') => PromptKey::Edit(PromptEdit::DeleteToStart),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            PromptKey::Edit(PromptEdit::Insert(c))
        }
        _ => PromptKey::Ignored,
    }
}

/// Handle everything about a prompt key that does not depend on the prompt, and
/// report what is left for the caller.
fn apply_shared_prompt_key(app: &mut App, key: KeyEvent) -> PromptKey {
    let classified = classify_prompt_key(key);
    match classified {
        PromptKey::Cancel => app.cancel_prompt(),
        PromptKey::Edit(edit) => {
            app.apply_prompt_edit(edit);
        }
        PromptKey::Submit | PromptKey::Previous | PromptKey::Next | PromptKey::Ignored => {}
    }
    classified
}

fn handle_open_workspace_key(app: &mut App, config: &Config, key: KeyEvent) {
    match apply_shared_prompt_key(app, key) {
        PromptKey::Submit => app.submit_open_workspace(&config.projects),
        PromptKey::Previous => app.select_previous_open_workspace_match(&config.projects),
        PromptKey::Next => app.select_next_open_workspace_match(&config.projects),
        PromptKey::Cancel | PromptKey::Edit(_) | PromptKey::Ignored => {}
    }
}

fn handle_terminal_command_key(app: &mut App, key: KeyEvent) {
    if apply_shared_prompt_key(app, key) == PromptKey::Submit {
        app.submit_new_terminal_command();
    }
}

fn handle_command_palette_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    frame_area: Rect,
) {
    match apply_shared_prompt_key(app, key) {
        PromptKey::Submit => {
            if let Some(action) = app.submit_command_palette() {
                execute_command_action(app, pty_runtime, config, store, action, frame_area);
            }
        }
        PromptKey::Previous => app.select_previous_command_palette_entry(),
        PromptKey::Next => app.select_next_command_palette_entry(),
        PromptKey::Cancel | PromptKey::Edit(_) | PromptKey::Ignored => {}
    }
}

fn handle_search_key(app: &mut App, key: KeyEvent) {
    if apply_shared_prompt_key(app, key) == PromptKey::Submit {
        app.submit_search();
    }
}

fn handle_delete_confirmation_key(app: &mut App, pty_runtime: &mut PtyRuntime, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.cancel_prompt(),
        KeyCode::Enter => confirm_pending_delete(app, pty_runtime),
        _ if is_unshifted_control_char(key, 'c') => app.cancel_prompt(),
        _ => {}
    }
}

fn execute_command_action(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    action: CommandAction,
    frame_area: Rect,
) {
    match action {
        CommandAction::ShowKeybindings => app.show_help(),
        CommandAction::DismissNotices => {
            app.dismiss_notices();
        }
        CommandAction::ReloadConfig => app.request_config_reload(),
        CommandAction::FocusSidebar => app.focus_sidebar(),
        CommandAction::FocusSelectedPane => {
            app.focus_selected_main();
        }
        CommandAction::StartInput => {
            focus_selected_input(app, pty_runtime, config, store, frame_area)
        }
        CommandAction::AddAgentChat => {
            add_agent_to_selected_workspace(
                app,
                pty_runtime,
                config,
                store,
                frame_area,
                AgentKind::Pi,
            );
        }
        CommandAction::AddClaudeCodeChat => {
            add_agent_to_selected_workspace(
                app,
                pty_runtime,
                config,
                store,
                frame_area,
                AgentKind::ClaudeCode,
            );
        }
        CommandAction::AddShellTerminal => app.add_terminal_to_selected_workspace(),
        CommandAction::AddCommandTerminal => {
            app.begin_new_terminal_command();
        }
        CommandAction::OpenWorkspace => app.begin_open_workspace(&config.projects),
        CommandAction::DeleteSelected => {
            app.begin_delete_selected();
        }
        CommandAction::SearchSelectedPane => {
            app.begin_search();
        }
        CommandAction::ClearSearch => app.clear_search(),
        CommandAction::Quit => app.quit(),
    }
}

fn start_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    let Some((workspace_id, terminal_id)) = app.selected_terminal_id() else {
        return;
    };

    start_terminal(
        app,
        pty_runtime,
        config,
        frame_area,
        workspace_id,
        terminal_id,
    );
}

fn start_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    _config: &Config,
    frame_area: Rect,
    workspace_id: model::WorkspaceId,
    terminal_id: model::TerminalId,
) -> bool {
    let key = PtyKey::Terminal(terminal_id);
    if pty_runtime.is_running(key) {
        pty_runtime.append_terminal_system_line(key, "PTY already running");
        return true;
    }

    let Some(workspace) = app.project.workspace(workspace_id) else {
        return false;
    };
    let Some(terminal) = workspace
        .terminals
        .iter()
        .find(|terminal| terminal.id == terminal_id)
    else {
        return false;
    };

    let terminal_name = terminal.name.clone();
    let Some(identity) = app.project.session_identity(key) else {
        pty_runtime.append_terminal_system_line(key, "durable terminal identity is missing");
        return false;
    };
    if let Err(error) = pty_runtime.register_session_identity(key, identity) {
        pty_runtime.append_terminal_system_line(
            key,
            format!("failed to register durable terminal identity: {error}"),
        );
        return false;
    }
    let mut spawn = match &terminal.launch {
        TerminalLaunch::Shell => {
            PtySpawn::shell(key, workspace.cwd.clone(), workspace.environment.clone())
        }
        TerminalLaunch::Command(command) => PtySpawn::command_line(
            key,
            command.clone(),
            workspace.cwd.clone(),
            workspace.environment.clone(),
        ),
    };
    spawn.size = terminal_dimensions(app, frame_area);

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.record_terminal_started(terminal_id);
            true
        }
        Err(error) => {
            pty_runtime.append_terminal_system_line(
                key,
                format!("failed to start terminal `{terminal_name}`: {error}"),
            );
            app.record_terminal_stopped(terminal_id);
            false
        }
    }
}

fn start_or_focus_selected_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    frame_area: Rect,
) {
    let Some((workspace_id, chat_id)) = app.selected_chat_id() else {
        return;
    };

    start_or_focus_chat_agent(
        app,
        pty_runtime,
        config,
        store,
        frame_area,
        ChatAgentLaunch {
            workspace_id,
            chat_id,
            focus_after_start: true,
        },
    );
}

/// Which chat to start, and whether the caller wants focus moved into it once
/// it is running. Grouped so the launch site reads as one decision rather than
/// three trailing positional arguments.
#[derive(Debug, Clone, Copy)]
struct ChatAgentLaunch {
    workspace_id: model::WorkspaceId,
    chat_id: model::ChatId,
    focus_after_start: bool,
}

fn start_or_focus_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    frame_area: Rect,
    launch: ChatAgentLaunch,
) {
    let ChatAgentLaunch {
        workspace_id,
        chat_id,
        focus_after_start,
    } = launch;
    let terminal_id = PtyKey::ChatAgent(chat_id);

    if pty_runtime.is_running(terminal_id) {
        if focus_after_start {
            app.begin_chat_agent_input();
        }
        return;
    }

    let Some(workspace) = app.project.workspace(workspace_id) else {
        return;
    };
    let (chat_name, agent, cwd, workspace_environment) = workspace
        .chats
        .iter()
        .find(|chat| chat.id == chat_id)
        .map(|chat| {
            (
                chat.name.clone(),
                chat.agent,
                workspace.cwd.clone(),
                workspace.environment.clone(),
            )
        })
        .unwrap_or_else(|| {
            (
                format!("chat {}", chat_id.0),
                AgentKind::default(),
                workspace.cwd.clone(),
                workspace.environment.clone(),
            )
        });
    let Some(identity) = app.project.session_identity(terminal_id) else {
        pty_runtime.append_terminal_system_line(terminal_id, "durable chat identity is missing");
        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
        return;
    };
    if let Err(error) = pty_runtime.register_session_identity(terminal_id, identity) {
        pty_runtime.append_terminal_system_line(
            terminal_id,
            format!("failed to register durable chat identity: {error}"),
        );
        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
        return;
    }

    // A persisted generation is restoration state, not permission to launch.
    // Reconcile it with the daemon using Attach only. If it is absent, persist
    // that fact before a later deliberate invocation may allocate a successor.
    if let Some(generation) = app.project.active_agent_generation(chat_id) {
        let metadata = agent_session_metadata(chat_id, agent, generation);
        if let Err(error) = pty_runtime.register_agent_session(terminal_id, metadata) {
            pty_runtime.append_terminal_system_line(
                terminal_id,
                format!("failed to register agent generation: {error}"),
            );
            app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
            return;
        }
        match pty_runtime.attach_existing(terminal_id, chat_agent_dimensions(app, frame_area)) {
            Ok(AttachExistingResult::Attached) => {
                reconcile_agent_status(app, pty_runtime, chat_id, agent, generation);
                if focus_after_start {
                    app.begin_chat_agent_input();
                }
                return;
            }
            Ok(AttachExistingResult::Missing) => {
                app.clear_agent_generation(chat_id, generation);
                app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
                if !persist_before_agent_launch(app, store) {
                    pty_runtime.append_terminal_system_line(
                        terminal_id,
                        "could not save missing agent generation; refusing to launch",
                    );
                    return;
                }
            }
            Err(error) => {
                app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
                pty_runtime.append_terminal_system_line(
                    terminal_id,
                    format!("failed to reconcile existing agent; refusing to relaunch: {error}"),
                );
                return;
            }
        }
    }

    let generation = match app.begin_agent_generation(chat_id) {
        Ok(Some(generation)) => generation,
        Ok(None) => return,
        Err(error) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
            pty_runtime.append_terminal_system_line(
                terminal_id,
                format!("failed to allocate secure agent generation: {error}"),
            );
            return;
        }
    };
    if !persist_before_agent_launch(app, store) {
        pty_runtime.append_terminal_system_line(
            terminal_id,
            "could not save agent generation; refusing to launch",
        );
        return;
    }

    let metadata = agent_session_metadata(chat_id, agent, generation);
    if let Err(error) = pty_runtime.register_agent_session(terminal_id, metadata) {
        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
        pty_runtime.append_terminal_system_line(
            terminal_id,
            format!("failed to register agent generation: {error}"),
        );
        return;
    }

    let command = agent_command(config, agent);
    let status_path = mult_agent_status_path(identity, generation);
    if let Err(error) = prepare_mult_agent_status_file(&status_path) {
        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
        pty_runtime.append_terminal_system_line(
            terminal_id,
            format!("failed to prepare private agent status journal: {error}"),
        );
        return;
    }
    let mut environment = workspace_environment;
    environment.insert(
        MULT_AGENT_STATUS_PATH_ENV.to_string(),
        status_path.display().to_string(),
    );
    environment.insert(MULT_AGENT_CHAT_ID_ENV.to_string(), chat_id.0.to_string());
    environment.insert(
        MULT_AGENT_STATUS_VERSION_ENV.to_string(),
        AGENT_STATUS_SCHEMA_VERSION.to_string(),
    );
    environment.insert(
        MULT_AGENT_NAMESPACE_ENV.to_string(),
        identity.namespace.to_string(),
    );
    environment.insert(
        MULT_AGENT_SESSION_TOKEN_ENV.to_string(),
        identity.token.to_string(),
    );
    environment.insert(
        MULT_AGENT_KIND_ENV.to_string(),
        agent_status_kind(agent).to_string(),
    );
    environment.insert(
        MULT_AGENT_GENERATION_ENV.to_string(),
        generation.to_string(),
    );
    let mut spawn = PtySpawn::command_line(terminal_id, command.clone(), cwd, environment);
    spawn.size = chat_agent_dimensions(app, frame_area);

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Idle);
            if focus_after_start {
                app.begin_chat_agent_input();
            }
        }
        Err(error) => {
            // Keep the saved generation: create delivery may be uncertain, and
            // a later attach is the only safe way to reconcile without a
            // duplicate command launch.
            app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
            pty_runtime.append_terminal_system_line(
                terminal_id,
                format!(
                    "failed to start {} agent for `{chat_name}`: {error}",
                    agent.display_name()
                ),
            );
        }
    }
}

/// An agent generation must be durable *before* the process that carries it
/// starts, so this save is forced: deferring it (B9) would risk a launched
/// agent whose generation no state file records.
fn persist_before_agent_launch(app: &mut App, store: &storage::StateStore) -> bool {
    save_if_dirty_with(app, true, |state| store.save(state));
    !app.is_dirty()
}

fn agent_session_metadata(
    chat: model::ChatId,
    agent: AgentKind,
    generation: model::AgentGeneration,
) -> AgentSessionMetadata {
    AgentSessionMetadata {
        schema_version: AGENT_STATUS_SCHEMA_VERSION,
        chat_id: chat.0,
        agent: wire_agent_kind(agent),
        generation: wire_agent_generation(generation),
    }
}

fn wire_agent_generation(generation: model::AgentGeneration) -> WireAgentGeneration {
    WireAgentGeneration::from_bytes(generation.as_bytes())
        .expect("durable agent generations are non-zero")
}

fn wire_agent_kind(agent: AgentKind) -> WireAgentKind {
    match agent {
        AgentKind::Pi => WireAgentKind::Pi,
        AgentKind::ClaudeCode => WireAgentKind::ClaudeCode,
    }
}

fn reconcile_agent_status(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    chat: model::ChatId,
    agent: AgentKind,
    generation: model::AgentGeneration,
) -> bool {
    let key = PtyKey::ChatAgent(chat);
    let Some(identity) = pty_runtime.registered_session_identity(key) else {
        return false;
    };
    let query = AgentStatusQuery {
        schema_version: AGENT_STATUS_SCHEMA_VERSION,
        identity,
        chat_id: chat.0,
        agent: wire_agent_kind(agent),
        generation: wire_agent_generation(generation),
    };
    match pty_runtime.get_agent_status(query) {
        Ok(Some(record)) if record.generation == query.generation => {
            app.mark_chat_status_by_id(chat, chat_status_from_agent_status(record.status));
            true
        }
        Ok(_) => false,
        Err(error) => {
            pty_runtime.append_terminal_system_line(
                key,
                format!("failed to reconcile daemon agent status: {error}"),
            );
            false
        }
    }
}

fn chat_status_from_agent_status(status: AgentStatus) -> ChatStatus {
    match status {
        AgentStatus::Idle => ChatStatus::Idle,
        AgentStatus::Running => ChatStatus::Thinking,
        AgentStatus::Waiting => ChatStatus::Waiting,
        AgentStatus::Finished | AgentStatus::Exited => ChatStatus::Done,
        AgentStatus::Failed => ChatStatus::Failed,
    }
}

fn auto_start_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) -> bool {
    if !config.auto_start_terminals || app.is_prompt_active() {
        return false;
    }

    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return false;
    };
    if app.terminal_requires_recovery(terminal_id) {
        return false;
    }
    let key = PtyKey::Terminal(terminal_id);
    if pty_runtime.is_running(key) || !pty_runtime.terminal_output_is_blank(key) {
        return false;
    }

    start_selected_terminal(app, pty_runtime, config, frame_area);
    true
}

fn auto_start_selected_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    frame_area: Rect,
) -> bool {
    if app.is_prompt_active() {
        return false;
    }

    let Some((workspace_id, chat_id)) = app.selected_chat_id() else {
        return false;
    };
    let agent = app
        .project
        .chat(workspace_id, chat_id)
        .map(|chat| chat.agent)
        .unwrap_or_default();
    if !auto_start_enabled(config, agent) {
        return false;
    }
    let terminal_id = PtyKey::ChatAgent(chat_id);
    if pty_runtime.is_running(terminal_id) || !pty_runtime.terminal_output_is_blank(terminal_id) {
        return false;
    }

    start_or_focus_chat_agent(
        app,
        pty_runtime,
        config,
        store,
        frame_area,
        ChatAgentLaunch {
            workspace_id,
            chat_id,
            focus_after_start: false,
        },
    );
    true
}

/// Whether the selected chat's agent should auto-start when its pane is
/// focused with a blank buffer. Each agent backend has its own toggle.
fn auto_start_enabled(config: &Config, agent: AgentKind) -> bool {
    match agent {
        AgentKind::Pi => config.auto_start_pi_agent,
        AgentKind::ClaudeCode => config.auto_start_claude_code_agent,
    }
}

/// Build the shell command line that backs a chat, chosen by its agent kind.
/// Both backends report status into the same per-chat file that `mult` polls,
/// but through different mechanisms: pi loads a bundled extension (`-e`), while
/// Claude Code gets a generated hooks settings file (`--settings`).
fn agent_command(config: &Config, agent: AgentKind) -> String {
    match agent {
        AgentKind::Pi => pi_command_with_mult_status_extension(config),
        AgentKind::ClaudeCode => claude_code_command_with_mult_status_hooks(config),
    }
}

fn pi_command(config: &Config) -> String {
    let command = config.pi_agent_command.trim();
    if command.is_empty() {
        "pi".to_string()
    } else {
        command.to_string()
    }
}

fn claude_code_command(config: &Config) -> String {
    let command = config.claude_code_command.trim();
    if command.is_empty() {
        "claude".to_string()
    } else {
        command.to_string()
    }
}

fn pi_command_with_mult_status_extension(config: &Config) -> String {
    let command = pi_command(config);
    let Some(extension) = write_mult_status_extension_file() else {
        return command;
    };

    format!(
        "{command} -e {}",
        quote_argument(&extension.display().to_string())
    )
}

/// Append `--settings <file>` pointing at a generated hooks file that reports
/// chat status into the file `mult` polls. `--settings` merges over the user's
/// own Claude Code settings for this session only, so it does not touch their
/// config on disk. If the files cannot be written, fall back to the plain
/// command — Claude Code still runs, just without a live status dot.
fn claude_code_command_with_mult_status_hooks(config: &Config) -> String {
    let command = claude_code_command(config);
    let Some(settings) = write_mult_claude_status_files() else {
        return command;
    };

    format!(
        "{command} --settings {}",
        quote_argument(&settings.display().to_string())
    )
}

fn write_mult_status_extension_file() -> Option<PathBuf> {
    let dir = ensure_mult_runtime_dir().ok()?;
    write_private_runtime_file(
        &dir,
        "mult-status-extension",
        "ts",
        MULT_STATUS_EXTENSION_SOURCE.as_bytes(),
    )
}

/// Write the bundled status-writer script and a Claude Code settings file whose
/// hooks invoke it, returning the settings path to hand to `--settings`. Two
/// files because the settings JSON must reference the script by absolute path.
fn write_mult_claude_status_files() -> Option<PathBuf> {
    let dir = ensure_mult_runtime_dir().ok()?;
    let script = write_private_runtime_file(
        &dir,
        "mult-claude-status",
        "sh",
        MULT_CLAUDE_STATUS_SCRIPT_SOURCE.as_bytes(),
    )?;
    let settings = mult_claude_status_settings_json(&script);
    write_private_runtime_file(&dir, "mult-claude-settings", "json", settings.as_bytes())
}

/// Build the Claude Code `--settings` JSON that maps lifecycle hook events to
/// `mult` statuses by invoking the bundled script with the status as its
/// argument. Built with `serde_json` so the script path is correctly escaped
/// into the embedded shell command.
fn mult_claude_status_settings_json(script: &Path) -> String {
    let script = quote_argument(&script.display().to_string());
    let hook = |status: &str| {
        serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": format!("sh {script} {status}") }],
        })
    };

    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [hook("idle")],
            "UserPromptSubmit": [hook("running")],
            "PreToolUse": [hook("running")],
            "Notification": [hook("waiting")],
            "Stop": [hook("finished")],
        },
    });

    serde_json::to_string(&settings).unwrap_or_default()
}

fn write_private_runtime_file(
    dir: &Path,
    prefix: &str,
    extension: &str,
    contents: &[u8],
) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    // Versioned fixed names bound generated artifacts to three files instead of
    // leaking a PID/random file on every command construction.
    let path = dir.join(format!("{prefix}-v2.{extension}"));
    rotate_legacy_runtime_artifacts(dir, prefix, &path).ok()?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(&path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid()
        || metadata.nlink() != 1
        || metadata.len() > 1024 * 1024
    {
        return None;
    }
    let _guard = RuntimeFileLock::try_acquire(&file)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .ok()?;
    let mut existing = Vec::new();
    file.read_to_end(&mut existing).ok()?;
    if existing != contents {
        file.set_len(0).ok()?;
        file.seek(SeekFrom::Start(0)).ok()?;
        file.write_all(contents).ok()?;
        file.sync_all().ok()?;
    }
    Some(path)
}

/// How many times a contended runtime artifact is retried, and how long each
/// wait is. Two `mult` instances only collide while one of them writes three
/// small files, so a bounded wait of a few milliseconds resolves virtually
/// every real contention — and never costs the render loop more than
/// [`RUNTIME_LOCK_ATTEMPTS`] × [`RUNTIME_LOCK_RETRY_DELAY`] (S5).
const RUNTIME_LOCK_ATTEMPTS: u32 = 5;
const RUNTIME_LOCK_RETRY_DELAY: Duration = Duration::from_millis(2);

/// An exclusive `flock` held for the length of a runtime-artifact write.
struct RuntimeFileLock(std::os::fd::RawFd);

impl RuntimeFileLock {
    /// Take the lock without ever blocking the render thread.
    ///
    /// A blocking `LOCK_EX` here meant a second `mult` starting an agent could
    /// stall the first one's whole event loop for as long as it took to write
    /// its files. `LOCK_NB` plus a bounded retry turns that into a short wait
    /// and, at worst, a caller that degrades (the agent starts without the
    /// status extension) instead of a frozen UI.
    fn try_acquire(file: &fs::File) -> Option<Self> {
        use std::os::fd::AsRawFd;

        let descriptor = file.as_raw_fd();
        for attempt in 0..RUNTIME_LOCK_ATTEMPTS {
            if unsafe { libc::flock(descriptor, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Some(Self(descriptor));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK)
                && error.raw_os_error() != Some(libc::EINTR)
            {
                return None;
            }
            if attempt + 1 < RUNTIME_LOCK_ATTEMPTS {
                std::thread::sleep(RUNTIME_LOCK_RETRY_DELAY);
            }
        }
        None
    }
}

impl Drop for RuntimeFileLock {
    fn drop(&mut self) {
        // The descriptor is closed right after this, which would release the
        // lock anyway; unlocking explicitly keeps the window closed even if the
        // file ever outlives the guard.
        unsafe { libc::flock(self.0, libc::LOCK_UN) };
    }
}

fn rotate_legacy_runtime_artifacts(
    directory: &Path,
    prefix: &str,
    current: &Path,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    const MAX_RETAINED_LEGACY_ARTIFACTS: usize = 16;
    let mut candidates = Vec::new();
    for entry in fs::read_dir(directory)?.take(4096) {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if path == current || !name.starts_with(prefix) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_file()
            && metadata.uid() == effective_uid()
            && metadata.nlink() == 1
        {
            candidates.push((metadata.modified().ok(), path));
        }
    }
    candidates.sort_by_key(|(modified, _)| *modified);
    let remove_count = candidates
        .len()
        .saturating_sub(MAX_RETAINED_LEGACY_ARTIFACTS);
    for (_, path) in candidates.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn focus_selected_input(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    frame_area: Rect,
) {
    if app.selected_chat_id().is_some() {
        start_or_focus_selected_chat_agent(app, pty_runtime, config, store, frame_area);
    } else if app.selected_terminal_id().is_some() {
        start_or_focus_selected_terminal(app, pty_runtime, config, frame_area);
    }
}

fn start_or_focus_selected_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    frame_area: Rect,
) {
    let Some((_, terminal_id)) = app.selected_terminal_id() else {
        return;
    };
    let key = PtyKey::Terminal(terminal_id);

    if !pty_runtime.is_running(key) {
        start_selected_terminal(app, pty_runtime, config, frame_area);
    }

    if pty_runtime.is_running(key) {
        app.begin_terminal_input();
    }
}

fn resize_visible_terminal(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    _config: &Config,
    frame_area: Rect,
) -> bool {
    let Some((terminal_id, area)) = ui::selected_terminal_output_area(app, frame_area) else {
        return false;
    };
    let size = pty_dimensions_from_area(area);
    let key = PtyKey::Terminal(terminal_id);
    resize_if_changed(pty_runtime, key, size)
}

fn resize_visible_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    _config: &Config,
    frame_area: Rect,
) -> bool {
    let Some((chat_id, area)) = ui::selected_chat_agent_output_area(app, frame_area) else {
        return false;
    };
    let terminal_id = PtyKey::ChatAgent(chat_id);
    let size = pty_dimensions_from_area(area);
    resize_if_changed(pty_runtime, terminal_id, size)
}

/// Resize `terminal` only when the size actually differs (D1).
///
/// Both callers run on every ~16 ms tick, and a `Resize` is not free at either
/// end: the client serializes and writes a message to the socket, and the
/// daemon takes the pane lock, takes the master lock and issues a
/// `TIOCSWINSZ`. Unconditionally resizing to the size the pane already has cost
/// ~125 writes/s per site at complete idle and changed nothing.
fn resize_if_changed(pty_runtime: &mut PtyRuntime, terminal: PtyKey, size: PtyDimensions) -> bool {
    if !pty_dimensions_changed(pty_runtime, terminal, size) {
        return false;
    }
    let _ = pty_runtime.resize(terminal, size);
    true
}

/// Whether resizing `terminal` to `size` would actually change its parser
/// dimensions (and therefore the rendered output). A terminal with no parser
/// yet is treated as changed so the freshly sized screen gets drawn.
fn pty_dimensions_changed(pty_runtime: &PtyRuntime, terminal: PtyKey, size: PtyDimensions) -> bool {
    match pty_runtime.parser(terminal) {
        Some(parser) => parser.screen().size() != (size.rows, size.cols),
        None => true,
    }
}

fn terminal_dimensions(app: &App, frame_area: Rect) -> PtyDimensions {
    pty_dimensions_from_area(ui::terminal_output_area_for(app, frame_area))
}

fn chat_agent_dimensions(app: &App, frame_area: Rect) -> PtyDimensions {
    pty_dimensions_from_area(ui::chat_agent_output_area_for(app, frame_area))
}

fn pty_dimensions_from_area(area: Rect) -> PtyDimensions {
    PtyDimensions {
        rows: area.height.max(1),
        cols: area.width.max(1),
    }
}

fn drain_pty_events(app: &mut App, pty_runtime: &mut PtyRuntime) -> bool {
    let mut changed = false;
    for event in pty_runtime.drain_events() {
        changed = true;
        apply_pty_event(app, pty_runtime, event);
    }
    // `drain_events` stops at a per-frame budget, so a busy pane can leave
    // traffic (or queued re-attachments) behind. Asking for a redraw keeps the
    // loop coming back for the rest instead of parking on a stale frame.
    changed || pty_runtime.has_pending_work()
}

fn apply_pty_event(app: &mut App, pty_runtime: &mut PtyRuntime, event: PtyEvent) {
    {
        match event {
            PtyEvent::Scrollback { .. } | PtyEvent::Output { .. } => {}
            // Truncation is metadata, not terminal output. Injecting a notice
            // into the parser here would place client text after replay/live
            // bytes and corrupt the daemon's exact byte ordering.
            PtyEvent::ReplayTruncated { .. } => {}
            PtyEvent::TakenOver { terminal } => {
                match terminal {
                    PtyKey::ChatAgent(chat_id) => {
                        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
                    }
                    PtyKey::Terminal(terminal_id) => {
                        app.record_terminal_stopped(terminal_id);
                    }
                }
                if app.pty_input_target() == Some(terminal) {
                    app.end_pty_input();
                }
                pty_runtime.append_terminal_system_line(
                    terminal,
                    "PTY attachment was taken over by another client",
                );
            }
            PtyEvent::Exited { terminal, status } => match terminal {
                PtyKey::ChatAgent(chat_id) => {
                    let chat_status = if status.code == 0 {
                        ChatStatus::Done
                    } else {
                        ChatStatus::Failed
                    };
                    let agent = chat_agent_kind(app, chat_id);
                    app.mark_chat_status_by_id(chat_id, chat_status);
                    if let (Some(identity), Some(generation)) = (
                        app.project.session_identity(terminal),
                        app.project.active_agent_generation(chat_id),
                    ) {
                        app.clear_agent_generation(chat_id, generation);
                        let _ = fs::remove_file(mult_agent_status_path(identity, generation));
                    }
                    if app.pty_input_target() == Some(terminal) {
                        app.end_pty_input();
                    }
                    let exit_message =
                        format!("{} agent exited: {}", agent.display_name(), status.label());
                    pty_runtime.append_terminal_system_line(terminal, exit_message.as_str());
                }
                PtyKey::Terminal(terminal_id) => {
                    app.record_terminal_stopped(terminal_id);
                    if app.terminal_input_target() == Some(terminal_id) {
                        app.end_pty_input();
                    }
                    let exit_message = format!("PTY exited: {}", status.label());
                    pty_runtime.append_terminal_system_line(terminal, exit_message.as_str());
                }
            },
            PtyEvent::Error { terminal, message } => {
                pty_runtime.append_terminal_system_line(terminal, message.as_str());
            }
            // No pane owns this, so there is no pane to write it into: a
            // missing or protocol-incompatible daemon otherwise left the user
            // with an inert UI and the explanation queued against a terminal
            // id that cannot exist (E2/B8).
            PtyEvent::ConnectionError { message } => {
                app.push_notice(NoticeLevel::Error, NoticeSource::Report, message);
            }
        }
    }
}

#[cfg(test)]
fn key_to_pty_bytes(key: KeyEvent) -> Vec<u8> {
    key_to_pty_bytes_in_mode(key, false)
}

fn key_to_pty_bytes_in_mode(key: KeyEvent, application_cursor: bool) -> Vec<u8> {
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

fn drain_agent_events(app: &mut App, backend: &mut impl AgentBackend) -> bool {
    let mut changed = false;
    for event in backend.drain_events() {
        changed = true;
        app.apply_agent_event(event);
    }
    changed
}

fn drain_mult_agent_status_events(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    bridge: &mut AgentStatusBridge<impl AgentStatusSource>,
    now: Instant,
) -> bool {
    if !bridge.is_due(now) {
        return false;
    }
    bridge.last_poll = Some(now);

    let chats = app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.chats.iter().filter_map(|chat| {
                let generation = app.project.active_agent_generation(chat.id)?;
                let identity = app.project.session_identity(PtyKey::ChatAgent(chat.id))?;
                Some((chat.id, chat.agent, identity, generation))
            })
        })
        .collect::<Vec<_>>();
    let live = chats.iter().map(|(chat, ..)| *chat).collect::<Vec<_>>();
    bridge.source.retain(&live);

    let mut changed = false;
    for (chat, agent, identity, generation) in chats {
        for record in bridge.source.poll(chat, identity, generation) {
            if !status_record_matches(&record, chat, agent, identity, generation) {
                continue;
            }
            let Some(status) = mult_agent_status(&record.status) else {
                continue;
            };
            let Some(wire_identity) =
                pty_runtime.registered_session_identity(PtyKey::ChatAgent(chat))
            else {
                break;
            };
            let update = AgentStatusRecord {
                schema_version: AGENT_STATUS_SCHEMA_VERSION,
                identity: wire_identity,
                chat_id: chat.0,
                agent: wire_agent_kind(agent),
                generation: wire_agent_generation(generation),
                status,
            };
            match pty_runtime.update_agent_status(update) {
                Ok(accepted) => {
                    changed |= app.mark_chat_status_by_id(
                        chat,
                        chat_status_from_agent_status(accepted.status),
                    );
                }
                Err(_) => {
                    // The daemon is authoritative. Reconcile a final status or
                    // stale generation instead of applying untrusted file data.
                    reconcile_agent_status(app, pty_runtime, chat, agent, generation);
                }
            }
        }
    }
    changed
}

const MAX_STATUS_RECORD_BYTES: usize = 4 * 1024;
const MAX_STATUS_FILE_BYTES: u64 = 1024 * 1024;

fn read_mult_agent_status_records(
    path: &Path,
    cursor: &mut AgentStatusCursor,
) -> io::Result<Vec<(MultAgentStatusRecord, u64)>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid()
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_STATUS_FILE_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "agent status journal failed type, owner, link, mode, or size validation",
        ));
    }

    if cursor.device != metadata.dev()
        || cursor.inode != metadata.ino()
        || cursor.offset > metadata.len()
    {
        cursor.device = metadata.dev();
        cursor.inode = metadata.ino();
        cursor.offset = 0;
    }
    file.seek(SeekFrom::Start(cursor.offset))?;
    let remaining = MAX_STATUS_FILE_BYTES.saturating_sub(cursor.offset);
    let mut bytes = Vec::new();
    file.take(remaining + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > remaining {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "agent status journal exceeds its byte limit",
        ));
    }

    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let mut records = Vec::new();
    let mut relative_offset = 0_u64;
    for line in bytes[..complete_len].split_inclusive(|byte| *byte == b'\n') {
        relative_offset = relative_offset.saturating_add(line.len() as u64);
        let encoded = line.strip_suffix(b"\n").unwrap_or(line);
        if encoded.is_empty() || encoded.len() > MAX_STATUS_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "agent status journal contains an empty or oversized record",
            ));
        }
        let record = serde_json::from_slice(encoded)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        records.push((record, cursor.offset + relative_offset));
    }
    Ok(records)
}

fn status_record_matches(
    record: &MultAgentStatusRecord,
    chat: model::ChatId,
    agent: AgentKind,
    identity: model::SessionIdentity,
    generation: model::AgentGeneration,
) -> bool {
    record.version == AGENT_STATUS_SCHEMA_VERSION
        && record.namespace == identity.namespace.to_string()
        && record.session_token == identity.token.to_string()
        && record.chat_id == chat.0.to_string()
        && record.agent_kind == agent_status_kind(agent)
        && record.generation == generation.to_string()
}

fn mult_agent_status(status: &str) -> Option<AgentStatus> {
    match status {
        "idle" => Some(AgentStatus::Idle),
        "running" => Some(AgentStatus::Running),
        "waiting" => Some(AgentStatus::Waiting),
        "error" => Some(AgentStatus::Failed),
        "finished" => Some(AgentStatus::Finished),
        _ => None,
    }
}

fn agent_status_kind(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Pi => "pi",
        AgentKind::ClaudeCode => "claude_code",
    }
}

/// The agent backend a chat runs, looked up by chat id alone (the durable model
/// keys chats under workspaces, but PTY events only carry the chat id).
fn chat_agent_kind(app: &App, chat_id: model::ChatId) -> AgentKind {
    app.project
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.chats.iter())
        .find(|chat| chat.id == chat_id)
        .map(|chat| chat.agent)
        .unwrap_or_default()
}

fn mult_agent_status_path(
    identity: model::SessionIdentity,
    generation: model::AgentGeneration,
) -> PathBuf {
    mult_runtime_dir().join("status-v1").join(format!(
        "{}-{}-{}.jsonl",
        identity.namespace, identity.token, generation
    ))
}

fn prepare_mult_agent_status_file(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "status journal has no parent")
    })?;
    mult_protocol::ensure_private_dir(parent)?;
    rotate_stale_status_files(parent, path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
    let file = options.open(path)?;
    file.sync_all()
}

fn rotate_stale_status_files(directory: &Path, current: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    const MAX_RETAINED_STATUS_FILES: usize = 256;
    let mut candidates = Vec::new();
    for entry in fs::read_dir(directory)?.take(4096) {
        let entry = entry?;
        let path = entry.path();
        if path == current || path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_file()
            && metadata.uid() == effective_uid()
            && metadata.nlink() == 1
        {
            candidates.push((metadata.modified().ok(), path));
        }
    }
    candidates.sort_by_key(|(modified, _)| *modified);
    let remove_count = candidates.len().saturating_sub(MAX_RETAINED_STATUS_FILES);
    for (_, path) in candidates.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn ensure_mult_runtime_dir() -> io::Result<PathBuf> {
    let dir = mult_runtime_dir();
    mult_protocol::ensure_private_dir(&dir)?;
    Ok(dir)
}

fn mult_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("mult-{}", effective_uid())))
        .join("mult")
}

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
struct SaveSchedule {
    last_save: Option<Instant>,
}

impl SaveSchedule {
    fn is_due(&self, now: Instant) -> bool {
        self.last_save
            .is_none_or(|last| now.saturating_duration_since(last) >= MIN_CONTENT_SAVE_INTERVAL)
    }

    fn record(&mut self, now: Instant) {
        self.last_save = Some(now);
    }
}

/// The rate-limited save the event loop runs every tick (B9). Structural
/// changes ignore the limit; everything else waits for the window.
fn save_content_if_due(
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

fn save_if_dirty_with(
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
    use std::{cell::Cell, os::unix::net::UnixListener, sync::mpsc, thread};

    use mult_protocol::{
        read_message, write_message, AttachError, AttachOutcome, AttachmentLease, ClientMessage,
        ClientScopeId, OutputSequence, PaneId, ServerInstanceId, ServerMessage, SessionId,
        PROTOCOL_VERSION,
    };

    use super::*;

    /// `Config` carries a private `warnings` list, so its fields cannot be
    /// filled with functional-update syntax from here.
    fn config_with(mutate: impl FnOnce(&mut Config)) -> Config {
        let mut config = Config::default();
        mutate(&mut config);
        config
    }

    #[derive(Clone, Copy)]
    enum RestorationReply {
        Attached,
        Missing,
    }

    /// A daemon that attaches every pane and records every client message until
    /// the client closes the socket. Unlike [`connected_restoration_runtime`]
    /// the server thread outlives the first request, so a test can assert on
    /// what the client sent *after* startup — join it only once the runtime has
    /// been dropped.
    fn recording_attached_runtime(
        terminal: model::TerminalId,
    ) -> (
        PtyRuntime,
        mpsc::Receiver<ClientMessage>,
        thread::JoinHandle<()>,
        PathBuf,
    ) {
        let socket_path = unique_status_path("recording").with_extension("sock");
        let _ = fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("bind recording test socket");
        let (observed_tx, observed_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept recording client");
            let hello: ClientMessage = read_message(&mut stream).expect("read client hello");
            assert!(matches!(hello, ClientMessage::Hello { .. }));
            write_message(
                &mut stream,
                &ServerMessage::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    server_instance: ServerInstanceId::from_bytes([1; 16]),
                    client_scope: ClientScopeId::from_bytes([2; 16]),
                    resumed: false,
                },
            )
            .expect("write server hello");

            let lease = AttachmentLease::MIN;
            let pane = PaneId(terminal.0);
            // Ends when the client drops its socket: the test is over.
            while let Ok(message) = read_message::<ClientMessage>(&mut stream) {
                if let ClientMessage::Attach { request_id, .. } = &message {
                    let request_id = *request_id;
                    for reply in [
                        ServerMessage::AttachResult {
                            request_id,
                            outcome: AttachOutcome::Attached {
                                session: SessionId(terminal.0),
                                pane: mult_protocol::PaneInfo {
                                    id: pane,
                                    title: "recorded".to_string(),
                                    rows: 40,
                                    cols: 86,
                                },
                                lease,
                            },
                        },
                        ServerMessage::ReplayBegin {
                            request_id,
                            pane,
                            lease,
                            first_sequence: OutputSequence::ZERO,
                            watermark: OutputSequence::ZERO,
                            omitted_prefix_bytes: 0,
                        },
                        ServerMessage::ReplayEnd {
                            request_id,
                            pane,
                            lease,
                            watermark: OutputSequence::ZERO,
                        },
                    ] {
                        write_message(&mut stream, &reply).expect("write attach reply");
                    }
                }
                if observed_tx.send(message).is_err() {
                    break;
                }
            }
        });
        let runtime =
            PtyRuntime::connect_to_socket(socket_path.clone()).expect("connect recording runtime");
        (runtime, observed_rx, server, socket_path)
    }

    /// D1: both resize sites ran on every ~16 ms tick and called `resize`
    /// unconditionally, so an idle session wrote a `Resize` to the socket ~125
    /// times a second — each one a pane lock, a master lock and a `TIOCSWINSZ`
    /// in the daemon for a size that had not changed.
    #[test]
    fn a_visible_pane_is_resized_only_when_its_size_changed() {
        let (mut app, _, terminal) = running_command_app("echo recorded".to_string());
        let (mut runtime, observed, server, socket_path) = recording_attached_runtime(terminal);
        let config = Config::default();
        let area = Rect::new(0, 0, 120, 40);

        restore_persisted_sessions(&mut app, &mut runtime, &config, area);
        // Settle the pane at the visible size, whatever the attach reported.
        resize_visible_terminal(&mut app, &mut runtime, &config, area);

        for _ in 0..8 {
            assert!(
                !resize_visible_terminal(&mut app, &mut runtime, &config, area),
                "an unchanged size is not a redraw reason either"
            );
        }
        // A genuine resize must still reach the daemon.
        assert!(resize_visible_terminal(
            &mut app,
            &mut runtime,
            &config,
            Rect::new(0, 0, 100, 30)
        ));

        drop(runtime);
        server.join().expect("recording server exits");
        let resizes = observed
            .into_iter()
            .filter_map(|message| match message {
                ClientMessage::Resize { rows, cols, .. } => Some((rows, cols)),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            !resizes.is_empty(),
            "a genuine resize must still reach the daemon"
        );
        // The eight idle ticks are the point: before the fix each one wrote the
        // size the pane already had.
        let distinct = resizes.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(
            distinct.len(),
            resizes.len(),
            "no size may be written twice: {resizes:?}"
        );
        assert!(
            resizes.len() <= 2,
            "at most one write per size: {resizes:?}"
        );
        let _ = fs::remove_file(socket_path);
    }

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

    fn connected_restoration_runtime(
        terminal: model::TerminalId,
        reply: RestorationReply,
    ) -> (
        PtyRuntime,
        mpsc::Receiver<ClientMessage>,
        thread::JoinHandle<()>,
        PathBuf,
    ) {
        let socket_path = unique_status_path("restore").with_extension("sock");
        let _ = fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("bind restoration test socket");
        let (observed_tx, observed_rx) = mpsc::sync_channel(1);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept restoration client");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("bound restoration server reads");
            let hello: ClientMessage = read_message(&mut stream).expect("read client hello");
            assert!(matches!(hello, ClientMessage::Hello { .. }));
            write_message(
                &mut stream,
                &ServerMessage::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    server_instance: ServerInstanceId::from_bytes([1; 16]),
                    client_scope: ClientScopeId::from_bytes([2; 16]),
                    resumed: false,
                },
            )
            .expect("write server hello");
            let message: ClientMessage =
                read_message(&mut stream).expect("read restoration request");
            let ClientMessage::Attach { request_id, .. } = message.clone() else {
                panic!("restoration must send Attach, got {message:?}");
            };
            observed_tx
                .send(message)
                .expect("report restoration request");
            match reply {
                RestorationReply::Attached => {
                    let lease = AttachmentLease::MIN;
                    write_message(
                        &mut stream,
                        &ServerMessage::AttachResult {
                            request_id,
                            outcome: AttachOutcome::Attached {
                                session: SessionId(terminal.0),
                                pane: mult_protocol::PaneInfo {
                                    id: PaneId(terminal.0),
                                    title: "restored".to_string(),
                                    rows: 40,
                                    cols: 86,
                                },
                                lease,
                            },
                        },
                    )
                    .expect("write attach result");
                    write_message(
                        &mut stream,
                        &ServerMessage::ReplayBegin {
                            request_id,
                            pane: PaneId(terminal.0),
                            lease,
                            first_sequence: OutputSequence::ZERO,
                            watermark: OutputSequence::ZERO,
                            omitted_prefix_bytes: 0,
                        },
                    )
                    .expect("write replay begin");
                    write_message(
                        &mut stream,
                        &ServerMessage::ReplayEnd {
                            request_id,
                            pane: PaneId(terminal.0),
                            lease,
                            watermark: OutputSequence::ZERO,
                        },
                    )
                    .expect("write replay end");
                }
                RestorationReply::Missing => {
                    write_message(
                        &mut stream,
                        &ServerMessage::AttachResult {
                            request_id,
                            outcome: AttachOutcome::Error(AttachError::SessionNotFound {
                                session: SessionId(terminal.0),
                            }),
                        },
                    )
                    .expect("write missing attach result");
                }
            }
        });
        let runtime = PtyRuntime::connect_to_socket(socket_path.clone())
            .expect("connect restoration runtime");
        (runtime, observed_rx, server, socket_path)
    }

    fn running_command_app(command: String) -> (App, model::WorkspaceId, model::TerminalId) {
        let mut state = model::ProjectState::try_first_run().expect("first-run project");
        let workspace = state.workspaces[0].id;
        let terminal = state.workspaces[0].terminals[0].id;
        let session = state
            .terminal_mut_by_id(terminal)
            .expect("default terminal exists");
        session.restore_on_launch = true;
        session.launch = TerminalLaunch::Command(command);
        let mut app = App::new(state);
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        (app, workspace, terminal)
    }

    #[test]
    fn restoration_attaches_to_an_existing_command_session_without_creating_it() {
        let (mut app, _, terminal) = running_command_app("echo restored".to_string());
        let (mut runtime, observed, server, socket_path) =
            connected_restoration_runtime(terminal, RestorationReply::Attached);

        restore_persisted_sessions(
            &mut app,
            &mut runtime,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        );

        assert!(matches!(
            observed
                .recv_timeout(Duration::from_secs(2))
                .expect("observe restoration request"),
            ClientMessage::Attach { session, .. } if session == SessionId(terminal.0)
        ));
        assert!(runtime.is_running(PtyKey::Terminal(terminal)));
        assert!(
            app.project
                .terminal_mut_by_id(terminal)
                .unwrap()
                .restore_on_launch
        );
        assert!(!app.terminal_requires_recovery(terminal));
        server.join().expect("restoration server exits");
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn missing_persisted_command_session_is_stopped_without_command_execution() {
        let side_effect = unique_status_path("must-not-run");
        let _ = fs::remove_file(&side_effect);
        let command = format!(
            "printf launched > {}",
            quote_argument(&side_effect.display().to_string())
        );
        let (mut app, workspace, terminal) = running_command_app(command);
        let (mut runtime, observed, server, socket_path) =
            connected_restoration_runtime(terminal, RestorationReply::Missing);

        restore_persisted_sessions(
            &mut app,
            &mut runtime,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        );

        assert!(matches!(
            observed
                .recv_timeout(Duration::from_secs(2))
                .expect("observe restoration request"),
            ClientMessage::Attach { .. }
        ));
        assert!(
            !side_effect.exists(),
            "restoration must not execute the command"
        );
        assert!(
            !app.project
                .terminal(workspace, terminal)
                .unwrap()
                .restore_on_launch
        );
        assert!(app.terminal_requires_recovery(terminal));

        // Saving the cleared restore intent and loading it again remains conservative:
        // a blank pane still cannot auto-start until deliberate user input.
        let mut reloaded = App::new(app.project.clone());
        reloaded.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        let offline_socket = unique_status_path("offline-restore").with_extension("sock");
        let mut offline = PtyRuntime::with_socket_path(offline_socket, SpawnPolicy::Autospawn);
        assert!(!auto_start_selected_terminal(
            &mut reloaded,
            &mut offline,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        ));
        assert!(!side_effect.exists());

        server.join().expect("restoration server exits");
        let _ = fs::remove_file(socket_path);
    }

    #[test]
    fn migrated_v1_command_restoration_uses_generated_identity_and_only_attach() {
        use std::os::unix::fs::DirBuilderExt;

        let root = unique_status_path("v1-migration").with_extension("");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .unwrap();
        let state_path = root.join("state.json");
        let side_effect = root.join("must-not-run");
        let command = format!(
            "printf launched > {}",
            quote_argument(&side_effect.display().to_string())
        );
        let v1 = serde_json::json!({
            "version": 1,
            "next_workspace_id": 2,
            "next_chat_id": 1,
            "next_terminal_id": 2,
            "workspaces": [{
                "id": 1,
                "name": "migrated",
                "cwd": null,
                "environment": {},
                "chats": [],
                "terminals": [{
                    "id": 1,
                    "name": "command",
                    "status": "Running",
                    "launch": { "kind": "command", "command": command }
                }]
            }]
        });
        fs::write(&state_path, serde_json::to_vec_pretty(&v1).unwrap()).unwrap();
        let store = storage::StateStore::acquire(
            storage::StatePaths::from_explicit_path(state_path.clone()).unwrap(),
        )
        .unwrap();
        let loaded = store.load_or_default().unwrap();
        assert!(loaded.needs_save);
        let terminal = loaded.state.workspaces[0].terminals[0].id;
        let expected_identity = loaded
            .state
            .session_identity(PtyKey::Terminal(terminal))
            .unwrap();
        store.save(&loaded.state).unwrap();
        let mut app = App::new(loaded.state);
        let (mut runtime, observed, server, socket_path) =
            connected_restoration_runtime(terminal, RestorationReply::Missing);

        restore_persisted_sessions(
            &mut app,
            &mut runtime,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        );

        let request = observed
            .recv_timeout(Duration::from_secs(2))
            .expect("migration restoration sends one request");
        let ClientMessage::Attach { identity, .. } = request else {
            panic!("migration restoration must send Attach only");
        };
        assert_eq!(
            identity.namespace.into_bytes(),
            expected_identity.namespace.as_bytes()
        );
        assert_eq!(
            identity.token.into_bytes(),
            expected_identity.token.as_bytes()
        );
        assert!(!side_effect.exists());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(state_path).unwrap()).unwrap()
                ["version"],
            model::STATE_VERSION
        );

        server.join().unwrap();
        let _ = fs::remove_file(socket_path);
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unreachable_daemon_marks_running_command_recoverable_without_execution() {
        let side_effect = unique_status_path("unreachable-must-not-run");
        let _ = fs::remove_file(&side_effect);
        let command = format!(
            "printf launched > {}",
            quote_argument(&side_effect.display().to_string())
        );
        let (mut app, workspace, terminal) = running_command_app(command);
        let socket = unique_status_path("unreachable-daemon").with_extension("sock");
        let mut runtime = PtyRuntime::with_socket_path(socket, SpawnPolicy::Autospawn);

        restore_persisted_sessions(
            &mut app,
            &mut runtime,
            &Config::default(),
            Rect::new(0, 0, 120, 40),
        );

        assert!(
            !app.project
                .terminal(workspace, terminal)
                .unwrap()
                .restore_on_launch
        );
        assert!(app.terminal_requires_recovery(terminal));
        assert!(!side_effect.exists());
    }

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

    /// An [`AgentStatusSource`] double (F10).
    ///
    /// The bridge's own logic — when it polls, which chats it keeps read state
    /// for, what it does with a record — has nothing to do with files, but
    /// testing it used to mean writing real journals with real modes into a
    /// real temporary directory. Records are queued per `(chat, generation)`,
    /// so a restarted agent is a fresh queue exactly as it is a fresh journal.
    #[derive(Default)]
    struct FakeAgentStatusSource {
        queued: HashMap<(model::ChatId, model::AgentGeneration), Vec<MultAgentStatusRecord>>,
        /// Chats this source is still holding read state for, newest call last.
        retained: Vec<model::ChatId>,
        polls: usize,
    }

    impl FakeAgentStatusSource {
        fn queue(
            &mut self,
            chat: model::ChatId,
            generation: model::AgentGeneration,
            record: MultAgentStatusRecord,
        ) {
            self.queued
                .entry((chat, generation))
                .or_default()
                .push(record);
        }
    }

    impl AgentStatusSource for FakeAgentStatusSource {
        fn poll(
            &mut self,
            chat: model::ChatId,
            _identity: model::SessionIdentity,
            generation: model::AgentGeneration,
        ) -> Vec<MultAgentStatusRecord> {
            self.polls += 1;
            self.queued.remove(&(chat, generation)).unwrap_or_default()
        }

        fn retain(&mut self, live: &[model::ChatId]) {
            self.queued.retain(|(chat, _), _| live.contains(chat));
            self.retained = live.to_vec();
        }
    }

    /// S3/B3: the status bridge used to `open`+`fstat`+`seek`+`read`+`close`
    /// every journal on every ~16 ms tick. The poll is now on a timer, and with
    /// the source behind a seam the timer can be tested without a filesystem
    /// at all (F10).
    #[test]
    fn agent_status_polling_is_timed() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let mut bridge = AgentStatusBridge::<FakeAgentStatusSource>::default();
        let start = Instant::now();

        assert!(bridge.is_due(start), "the first tick always polls");
        drain_mult_agent_status_events(&mut app, &mut pty_runtime, &mut bridge, start);
        assert!(
            !bridge.is_due(start + AGENT_STATUS_POLL_INTERVAL / 2),
            "a tick inside the interval must not touch the source"
        );
        assert!(bridge.is_due(start + AGENT_STATUS_POLL_INTERVAL));
    }

    /// A chat that stops, or is deleted, must not leave read state behind for a
    /// later chat to inherit. The double records exactly which chats the bridge
    /// declared live, which the file-backed source used to hide behind a
    /// `HashMap` of paths.
    #[test]
    fn a_chat_without_a_live_agent_session_keeps_no_read_state() {
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let mut bridge = AgentStatusBridge::<FakeAgentStatusSource>::default();
        let (_, chat) = app
            .add_chat_to_selected_workspace_and_return(model::AgentKind::Pi)
            .or_else(|| {
                app.select_next();
                app.add_chat_to_selected_workspace_and_return(model::AgentKind::Pi)
            })
            .expect("a chat in the default project");

        // No generation yet: the chat exists but owns no agent session.
        drain_mult_agent_status_events(&mut app, &mut pty_runtime, &mut bridge, Instant::now());
        assert!(
            bridge.source.retained.is_empty(),
            "a chat with no active generation is not a live source"
        );

        let generation = app
            .begin_agent_generation(chat)
            .expect("allocate generation")
            .expect("a generation for a known chat");
        drain_mult_agent_status_events(
            &mut app,
            &mut pty_runtime,
            &mut bridge,
            Instant::now() + AGENT_STATUS_POLL_INTERVAL,
        );
        assert_eq!(bridge.source.retained, vec![chat]);

        // A queued record for a *different* generation belongs to a restarted
        // agent and is never read as this session's.
        let other = model::AgentGeneration::from_bytes([9; 16]).expect("non-zero generation");
        assert_ne!(other, generation);
        bridge.source.queue(chat, other, status_record("running"));
        drain_mult_agent_status_events(
            &mut app,
            &mut pty_runtime,
            &mut bridge,
            Instant::now() + AGENT_STATUS_POLL_INTERVAL * 2,
        );
        assert_eq!(
            app.project
                .workspaces
                .iter()
                .flat_map(|workspace| workspace.chats.iter())
                .find(|session| session.id == chat)
                .map(|session| session.status),
            Some(ChatStatus::Idle),
            "another generation's record cannot move this chat"
        );
    }

    fn status_record(status: &str) -> MultAgentStatusRecord {
        MultAgentStatusRecord {
            version: mult_protocol::AGENT_STATUS_SCHEMA_VERSION,
            namespace: String::new(),
            session_token: String::new(),
            chat_id: String::new(),
            agent_kind: "pi".to_string(),
            generation: String::new(),
            status: status.to_string(),
        }
    }

    /// The file-backed source keeps its own tests against a real filesystem:
    /// symlink refusal, mode and size limits are a security boundary, and a
    /// double would test nothing about them. What moved to the double is the
    /// bridge logic above, which never had any business opening a file.
    #[test]
    fn the_journal_source_caches_a_path_per_agent_session() {
        let mut source = JournalStatusSource::default();
        let chat = model::ChatId(7);
        let identity = model::ProjectState::try_first_run()
            .expect("first-run project")
            .session_identity(PtyKey::Terminal(model::TerminalId(1)))
            .expect("the default project has a terminal identity");
        let first_generation = model::AgentGeneration::from_bytes([3; 16]).unwrap();
        let second_generation = model::AgentGeneration::from_bytes([4; 16]).unwrap();

        let path = source
            .journal_for(chat, identity, first_generation)
            .path
            .clone();
        source
            .journal_for(chat, identity, first_generation)
            .cursor
            .offset = 42;
        assert_eq!(
            source.journal_for(chat, identity, first_generation).path,
            path,
            "an unchanged session reuses the cached path"
        );
        assert_eq!(
            source
                .journal_for(chat, identity, first_generation)
                .cursor
                .offset,
            42,
            "and keeps its read cursor"
        );

        let restarted = source.journal_for(chat, identity, second_generation);
        assert_ne!(restarted.path, path, "a new generation names a new journal");
        assert_eq!(restarted.cursor.offset, 0, "and is read from the beginning");
        assert_eq!(source.journals.len(), 1, "one entry per chat, not per tick");

        source.retain(&[]);
        assert!(source.journals.is_empty(), "a dead chat keeps no cursor");
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

    /// S5: a second `mult` writing the same runtime artifact must not stall
    /// this one's render loop. The lock is taken non-blocking with a bounded
    /// retry, so contention degrades (no status extension) instead of hanging.
    #[test]
    fn a_contended_runtime_artifact_lock_gives_up_instead_of_blocking() {
        use std::os::unix::fs::OpenOptionsExt;

        let directory = unique_status_path("runtime-lock").with_extension("dir");
        fs::create_dir_all(&directory).expect("create runtime artifact directory");
        let path = directory.join("mult-status-extension-v2.ts");
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .expect("create contended artifact");
        assert_eq!(
            unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&holder),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            },
            0,
            "the test takes the lock the other instance would hold"
        );

        assert!(
            write_private_runtime_file(&directory, "mult-status-extension", "ts", b"source")
                .is_none(),
            "a contended write reports failure instead of blocking the render loop"
        );

        drop(holder);
        assert_eq!(
            write_private_runtime_file(&directory, "mult-status-extension", "ts", b"source")
                .as_deref(),
            Some(path.as_path()),
            "and succeeds once the other instance is done"
        );
        let _ = fs::remove_dir_all(&directory);
    }

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

    #[test]
    fn delete_stop_failure_keeps_the_target_and_confirmation_open() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        app.mark_clean();
        assert!(app.begin_delete_selected());

        let removed = confirm_pending_delete_with(&mut app, |current, key| {
            assert_eq!(key, PtyKey::Terminal(terminal));
            assert!(current.project.terminal(workspace, terminal).is_some());
            Err(Box::new(io::Error::other("daemon refused stop")))
        });

        assert!(removed.is_empty());
        assert!(app.project.terminal(workspace, terminal).is_some());
        assert!(!app.is_dirty());
        assert!(matches!(
            app.prompt,
            Some(Prompt::ConfirmDelete(ref prompt))
                if prompt.error.as_deref().is_some_and(|error| error.contains("daemon refused stop"))
        ));
    }

    #[test]
    fn successful_delete_stops_before_mutating_project_state() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        assert!(app.begin_delete_selected());

        let removed = confirm_pending_delete_with(&mut app, |current, key| {
            assert_eq!(key, PtyKey::Terminal(terminal));
            assert!(current.project.terminal(workspace, terminal).is_some());
            Ok(())
        });

        assert_eq!(removed, vec![PtyKey::Terminal(terminal)]);
        assert!(app.project.terminal(workspace, terminal).is_none());
    }

    #[test]
    fn process_agent_command_parses_from_env_style_string() {
        let command =
            parse_process_agent_command("agent-cli --model local").expect("command parses");

        assert_eq!(command.program, "agent-cli");
        assert_eq!(command.args, vec!["--model", "local"]);
        assert_eq!(command.label(), "agent-cli --model local");
    }

    #[test]
    fn process_agent_command_supports_basic_shell_quoting() {
        let command = parse_process_agent_command(
            "agent-cli --prompt 'hello world' \"two words\" escaped\\ space",
        )
        .expect("command parses");

        assert_eq!(command.program, "agent-cli");
        assert_eq!(
            command.args,
            vec!["--prompt", "hello world", "two words", "escaped space"]
        );
    }

    #[test]
    fn blank_or_unterminated_process_agent_command_is_ignored() {
        assert_eq!(parse_process_agent_command("   "), None);
        assert_eq!(parse_process_agent_command("agent 'unterminated"), None);
    }

    #[test]
    fn read_mult_agent_status_parses_complete_records_and_tolerates_a_torn_tail() {
        let path = unique_status_path("small");
        write_private_status(&path, b"{\"version\":1,\"namespace\":\"n\",\"sessionToken\":\"t\",\"chatId\":\"7\",\"agentKind\":\"pi\",\"generation\":\"g\",\"status\":\"running\"}\n{\"version\":1").unwrap();
        let mut cursor = AgentStatusCursor::default();

        let records = read_mult_agent_status_records(&path, &mut cursor).unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0.status, "running");
        assert!(records[0].1 < fs::metadata(&path).unwrap().len());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_mult_agent_status_rejects_oversized_files() {
        let path = unique_status_path("huge");
        write_private_status(&path, &vec![b'x'; MAX_STATUS_FILE_BYTES as usize + 1]).unwrap();

        let error = read_mult_agent_status_records(&path, &mut AgentStatusCursor::default())
            .expect_err("oversized journal must fail");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_mult_agent_status_rejects_group_readable_files() {
        use std::os::unix::fs::PermissionsExt;
        let path = unique_status_path("mode");
        write_private_status(&path, b"{}\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        assert!(read_mult_agent_status_records(&path, &mut AgentStatusCursor::default()).is_err());

        let _ = fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn read_mult_agent_status_does_not_follow_symlinks() {
        let target = unique_status_path("symlink-target");
        write_private_status(&target, b"{}\n").unwrap();
        let link = unique_status_path("symlink-link");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        assert!(read_mult_agent_status_records(&link, &mut AgentStatusCursor::default()).is_err());

        let _ = fs::remove_file(&link);
        let _ = fs::remove_file(&target);
    }

    fn unique_status_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "mult-status-test-{label}-{}-{nanos}.json",
            std::process::id()
        ))
    }

    /// A state store over a fresh private directory.
    ///
    /// Input handling reaches the agent-launch path, which persists through the
    /// locked store (B16), so tests that drive keys need one even when they
    /// never save.
    fn test_state_store(label: &str) -> storage::StateStore {
        let path = unique_status_path(label)
            .with_extension("store")
            .join("state.json");
        storage::StateStore::acquire(
            storage::StatePaths::from_explicit_path(path).expect("test state path"),
        )
        .expect("acquire test state store")
    }

    fn write_private_status(path: &Path, contents: &[u8]) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)
    }

    #[test]
    fn restored_terminal_dimensions_use_visible_main_width_even_when_not_selected() {
        let app = App::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        assert_eq!(
            terminal_dimensions(&app, frame_area),
            PtyDimensions { rows: 40, cols: 86 }
        );
    }

    #[test]
    fn pi_command_comes_from_config_with_default_fallback() {
        assert_eq!(
            pi_command(&config_with(|config| {
                config.pi_agent_command = "pi -c".to_string()
            })),
            "pi -c"
        );
        assert_eq!(
            pi_command(&config_with(|config| {
                config.pi_agent_command = "   ".to_string()
            })),
            "pi"
        );
    }

    #[test]
    fn claude_code_command_comes_from_config_with_default_fallback() {
        assert_eq!(
            claude_code_command(&config_with(|config| {
                config.claude_code_command = "claude --resume".to_string()
            })),
            "claude --resume"
        );
        assert_eq!(
            claude_code_command(&config_with(|config| {
                config.claude_code_command = "   ".to_string()
            })),
            "claude"
        );
    }

    #[test]
    fn pi_command_appends_mult_status_extension_when_available() {
        let command = pi_command_with_mult_status_extension(&config_with(|config| {
            config.pi_agent_command = "pi --model test".to_string()
        }));

        assert!(command.starts_with("pi --model test"));
        assert!(command.contains(" -e "));
        assert!(command.contains("mult-status-extension-"));
    }

    #[test]
    fn agent_command_routes_by_kind() {
        let config = config_with(|config| {
            config.pi_agent_command = "pi".to_string();
            config.claude_code_command = "claude --here".to_string();
        });

        // Pi takes the bundled status extension (`-e`); Claude Code takes a
        // generated hooks settings file (`--settings`). Neither borrows the
        // other's flag.
        let pi = agent_command(&config, AgentKind::Pi);
        assert!(pi.starts_with("pi"));
        assert!(pi.contains(" -e "));
        assert!(!pi.contains(" --settings "));

        let cc = agent_command(&config, AgentKind::ClaudeCode);
        assert!(cc.starts_with("claude --here"));
        assert!(cc.contains(" --settings "));
        assert!(!cc.contains(" -e "));
    }

    #[test]
    fn claude_code_command_appends_mult_status_hooks_when_available() {
        let command = claude_code_command_with_mult_status_hooks(&config_with(|config| {
            config.claude_code_command = "claude --model test".to_string()
        }));

        assert!(command.starts_with("claude --model test"));
        assert!(command.contains(" --settings "));
        assert!(command.contains("mult-claude-settings-"));
    }

    #[test]
    fn mult_claude_status_settings_json_maps_each_event() {
        let json = mult_claude_status_settings_json(Path::new("/run/mult/status.sh"));
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid settings json");

        // Each lifecycle event registers one matcher-less command hook that runs
        // the bundled script with the mult status for that event.
        for (event, status) in [
            ("SessionStart", "idle"),
            ("UserPromptSubmit", "running"),
            ("PreToolUse", "running"),
            ("Notification", "waiting"),
            ("Stop", "finished"),
        ] {
            let entry = &value["hooks"][event][0];
            assert_eq!(entry["matcher"], "");
            let command = entry["hooks"][0]["command"]
                .as_str()
                .expect("command is a string");
            assert_eq!(entry["hooks"][0]["type"], "command");
            assert!(
                command.starts_with("sh /run/mult/status.sh "),
                "unexpected command for {event}: {command}"
            );
            assert!(
                command.ends_with(&format!(" {status}")),
                "event {event} should map to status {status}, got {command}"
            );
        }
    }

    // The two halves of the feature must agree on the file schema: the bundled
    // shell script has to write exactly what `read_mult_agent_status` parses.
    #[cfg(unix)]
    #[test]
    fn bundled_claude_status_script_writes_a_status_mult_can_read() {
        let script = unique_status_path("cc-script");
        fs::write(&script, MULT_CLAUDE_STATUS_SCRIPT_SOURCE).expect("write script");
        let status_path = unique_status_path("cc-status");
        let _ = fs::remove_file(&status_path);
        write_private_status(&status_path, b"").unwrap();

        let output = std::process::Command::new("sh")
            .arg(&script)
            .arg("running")
            .env(MULT_AGENT_STATUS_PATH_ENV, &status_path)
            .env(MULT_AGENT_STATUS_VERSION_ENV, "1")
            .env(MULT_AGENT_NAMESPACE_ENV, "namespace")
            .env(MULT_AGENT_SESSION_TOKEN_ENV, "token")
            .env(MULT_AGENT_CHAT_ID_ENV, "7")
            .env(MULT_AGENT_KIND_ENV, "pi")
            .env(MULT_AGENT_GENERATION_ENV, "generation")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run status script");
        assert!(output.status.success());

        let records =
            read_mult_agent_status_records(&status_path, &mut AgentStatusCursor::default())
                .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0.status, "running");

        let _ = fs::remove_file(&script);
        let _ = fs::remove_file(&status_path);
    }

    #[test]
    fn status_record_validation_binds_every_identity_field_and_generation() {
        let mut state = model::ProjectState::try_first_run().expect("first-run project");
        let workspace = state.workspaces[0].id;
        let chat = state
            .add_chat(
                workspace,
                model::DEFAULT_AGENT_CHAT_TITLE.to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .unwrap()
            .unwrap();
        let generation = state.begin_agent_generation(chat).unwrap().unwrap();
        let identity = state.session_identity(PtyKey::ChatAgent(chat)).unwrap();
        let encoded = format!(
            "{{\"version\":1,\"namespace\":\"{}\",\"sessionToken\":\"{}\",\"chatId\":\"{}\",\"agentKind\":\"pi\",\"generation\":\"{}\",\"status\":\"finished\"}}",
            identity.namespace, identity.token, chat.0, generation
        );
        let record: MultAgentStatusRecord = serde_json::from_str(&encoded).unwrap();

        assert!(status_record_matches(
            &record,
            chat,
            AgentKind::Pi,
            identity,
            generation
        ));
        assert_eq!(
            mult_agent_status(&record.status).map(chat_status_from_agent_status),
            Some(ChatStatus::Done)
        );
        assert!(!status_record_matches(
            &record,
            model::ChatId(chat.0 + 1),
            AgentKind::Pi,
            identity,
            generation
        ));
    }

    #[test]
    fn ctrl_j_and_ctrl_k_navigate_selection() {
        let store = test_state_store("ctrl-j-and-ctrl-k-navigate-selection");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(app.selected_index(), Some(1));
        assert_eq!(app.selected_item(), Some(app.nav_items()[1]));

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(app.selected_index(), Some(0));
        assert_eq!(app.selected_item(), Some(app.nav_items()[0]));
    }

    #[test]
    fn ctrl_p_opens_palette_and_ctrl_s_opens_search_for_selected_pane() {
        let store = test_state_store("ctrl-p-opens-palette-and-ctrl-s-opens-se");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::CommandPalette(_))));
        app.cancel_prompt();

        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        let target = NavItem::Terminal {
            workspace,
            terminal,
        };
        app.select_item(target);
        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::Search(_))));
    }

    #[test]
    fn plain_keys_are_not_workspace_commands() {
        let store = test_state_store("plain-keys-are-not-workspace-commands");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            frame_area,
        );
        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            frame_area,
        );

        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
        assert!(!app.should_quit);
        assert_eq!(app.prompt, None);
    }

    #[test]
    fn mouse_wheel_scrolls_output_under_cursor() {
        let store = test_state_store("mouse-wheel-scrolls-output-under-cursor");
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                mult::app::NavItem::Terminal { terminal, .. } => {
                    Some((index, PtyKey::Terminal(*terminal)))
                }
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let config = config_with(|config| config.mouse_capture = true);
        let frame_area = Rect::new(0, 0, 120, 40);
        let (_, output_area) = ui::selected_terminal_output_area(&app, frame_area)
            .expect("terminal selection has output area");

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        assert_eq!(
            pty_runtime.terminal_lines(terminal_id),
            vec!["one".to_string(), "two".to_string()]
        );

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        assert_eq!(
            pty_runtime.terminal_lines(terminal_id),
            vec!["four".to_string(), "five".to_string()]
        );
    }

    #[test]
    fn mouse_wheel_does_not_scroll_local_buffer_when_program_grabs_mouse() {
        let store = test_state_store("mouse-wheel-does-not-scroll-local-buffer");
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                mult::app::NavItem::Terminal { terminal, .. } => {
                    Some((index, PtyKey::Terminal(*terminal)))
                }
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        // The program turns on mouse reporting: the wheel is now its input, so
        // our local scrollback must stay pinned to the bottom.
        pty_runtime.process_terminal_output(terminal_id, b"\x1b[?1000h\x1b[?1006h");
        let config = config_with(|config| config.mouse_capture = true);
        let frame_area = Rect::new(0, 0, 120, 40);
        let (_, output_area) = ui::selected_terminal_output_area(&app, frame_area)
            .expect("terminal selection has output area");

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );

        assert_eq!(
            pty_runtime
                .parser(terminal_id)
                .unwrap()
                .screen()
                .scrollback(),
            0
        );
        assert_eq!(
            pty_runtime.terminal_lines(terminal_id),
            vec!["four".to_string(), "five".to_string()]
        );
    }

    #[test]
    fn mouse_wheel_scroll_moves_text_selection_with_scrollback() {
        let store = test_state_store("mouse-wheel-scroll-moves-text-selection-");
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                mult::app::NavItem::Terminal { terminal, .. } => {
                    Some((index, PtyKey::Terminal(*terminal)))
                }
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 0, col: 2 });

        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let config = config_with(|config| config.mouse_capture = true);
        let frame_area = Rect::new(0, 0, 120, 40);
        let (_, output_area) = ui::selected_terminal_output_area(&app, frame_area)
            .expect("terminal selection has output area");

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        let selection = app
            .text_selection_for(terminal_id)
            .expect("selection follows scroll up");
        assert_eq!(selection.anchor.row, 3);
        assert_eq!(selection.focus.row, 3);

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            frame_area,
        );
        let selection = app
            .text_selection_for(terminal_id)
            .expect("selection follows scroll down");
        assert_eq!(selection.anchor.row, 0);
        assert_eq!(selection.focus.row, 0);
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
            selected_text(&pty_runtime, selection).as_deref(),
            Some("bc\nd")
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

        let select = |start: u16, end: u16| {
            selected_text(
                &pty_runtime,
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

    #[test]
    fn ctrl_keys_create_delete_and_quit() {
        let store = test_state_store("ctrl-keys-create-delete-and-quit");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(
                KeyCode::Char('Q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            frame_area,
        );
        assert!(!app.should_quit);

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(app.should_quit);
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        app.cancel_quit();
        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::ConfirmDelete(_))));
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt, None);
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            frame_area,
        );
        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt, None);
        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
    }

    #[test]
    fn ctrl_x_adds_a_claude_code_agent_chat() {
        let store = test_state_store("ctrl-x-adds-a-claude-code-agent-chat");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let workspace = app.project.workspaces[0].id;
        assert!(app.project.workspaces[0].chats.is_empty());

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            frame_area,
        );

        // Ctrl+x adds and selects a chat backed by Claude Code, distinct from
        // the pi chat that Ctrl+a creates.
        assert_eq!(app.project.workspaces[0].chats.len(), 1);
        assert_eq!(
            app.project.workspaces[0].chats[0].agent,
            AgentKind::ClaudeCode
        );
        let chat = app.project.workspaces[0].chats[0].id;
        assert_eq!(app.selected_item(), Some(NavItem::Chat { workspace, chat }));
    }

    #[test]
    fn ctrl_c_is_not_a_command_terminal_shortcut() {
        let store = test_state_store("ctrl-c-is-not-a-command-terminal-shortcu");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            frame_area,
        );

        assert_eq!(app.prompt, None);
        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
    }

    #[test]
    fn ctrl_shift_c_is_copy_shortcut_not_pty_interrupt() {
        let store = test_state_store("ctrl-shift-c-is-copy-shortcut-not-pty-in");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let key = KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );

        assert!(handle_control_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            key,
            frame_area,
        ));
        assert!(key_to_pty_bytes(key).is_empty());
        assert!(key_to_pty_bytes(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .is_empty());
    }

    #[test]
    fn ctrl_f_opens_workspace_prompt() {
        let store = test_state_store("ctrl-f-opens-workspace-prompt");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::OpenWorkspace(_))));
    }

    #[test]
    fn open_workspace_prompt_ctrl_j_and_ctrl_k_select_matches() {
        let mut app = App::default();
        let config = config_with(|config| {
            config.projects = vec![
                mult::config::ConfiguredProject {
                    name: "first".to_string(),
                    path: "/tmp/first".into(),
                },
                mult::config::ConfiguredProject {
                    name: "second".to_string(),
                    path: "/tmp/second".into(),
                },
            ];
        });

        app.begin_open_workspace(&config.projects);
        handle_open_workspace_key(
            &mut app,
            &config,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            app.prompt,
            Some(Prompt::OpenWorkspace(ref prompt)) if prompt.selected.index() == 1
        ));

        handle_open_workspace_key(
            &mut app,
            &config,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            app.prompt,
            Some(Prompt::OpenWorkspace(ref prompt)) if prompt.selected.index() == 0
        ));
    }

    // ---- E7 / F13: one prompt-key path ---------------------------------

    /// One prompt's key handler, boxed so several of them can be driven by a
    /// single loop. Named because the tuple it sits in is otherwise too dense
    /// to read (and `clippy::type_complexity` says so).
    type PromptDrive = Box<dyn FnMut(&mut App, KeyEvent)>;

    /// Every text prompt shares one classifier, so the motions and kills are
    /// present in all of them rather than in whichever one was edited last.
    #[test]
    fn prompt_motions_and_kills_work_in_every_text_prompt() {
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let ctrl = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);

        // Two prompts with a list and two without, driven through their own
        // handlers exactly as `handle_key` dispatches them.
        let mut drives: Vec<(&str, PromptDrive)> = vec![
            (
                "open workspace",
                Box::new(move |app: &mut App, event| {
                    handle_open_workspace_key(app, &Config::default(), event)
                }),
            ),
            (
                "new terminal command",
                Box::new(|app: &mut App, event| handle_terminal_command_key(app, event)),
            ),
            (
                "search",
                Box::new(|app: &mut App, event| handle_search_key(app, event)),
            ),
        ];

        for (name, drive) in &mut drives {
            let mut app = App::default();
            match *name {
                "open workspace" => app.begin_open_workspace(&config.projects),
                "new terminal command" => {
                    assert!(app.begin_new_terminal_command(), "{name}");
                }
                _ => assert!(app.begin_search(), "{name}"),
            }
            // Start from a known state: the Open-Workspace prompt pre-fills the
            // working directory.
            while app.prompt_input().is_some_and(|input| !input.is_empty()) {
                drive(&mut app, ctrl('u'));
                drive(&mut app, key(KeyCode::Delete));
            }

            for ch in "cargo test".chars() {
                drive(&mut app, key(KeyCode::Char(ch)));
            }
            assert_eq!(app.prompt_input().unwrap().as_str(), "cargo test", "{name}");

            drive(&mut app, key(KeyCode::Home));
            assert_eq!(app.prompt_input().unwrap().cursor(), 0, "{name}");
            drive(&mut app, key(KeyCode::Delete));
            assert_eq!(app.prompt_input().unwrap().as_str(), "argo test", "{name}");
            drive(&mut app, key(KeyCode::End));
            drive(&mut app, ctrl('w'));
            assert_eq!(app.prompt_input().unwrap().as_str(), "argo ", "{name}");
            drive(&mut app, key(KeyCode::Left));
            drive(&mut app, key(KeyCode::Char('X')));
            assert_eq!(app.prompt_input().unwrap().as_str(), "argoX ", "{name}");
            drive(&mut app, ctrl('a'));
            assert_eq!(app.prompt_input().unwrap().cursor(), 0, "{name}");
            drive(&mut app, ctrl('e'));
            assert_eq!(app.prompt_input().unwrap().cursor(), 6, "{name}");
            drive(&mut app, ctrl('u'));
            assert_eq!(app.prompt_input().unwrap().as_str(), "", "{name}");

            // ...and cancelling is the same key everywhere.
            drive(&mut app, ctrl('c'));
            assert!(app.prompt.is_none(), "{name}");
        }
    }

    /// `Ctrl+k` keeps meaning "previous", not readline's kill-to-end-of-line.
    /// See `classify_prompt_key` for why: one key, one meaning, across every
    /// prompt — `Ctrl+u`/`Ctrl+w`/`Delete` are the deletions.
    #[test]
    fn ctrl_k_moves_the_selection_and_never_kills_the_line() {
        let mut app = App::default();
        app.begin_command_palette();
        for ch in "focus".chars() {
            app.push_prompt_char(ch);
        }
        let store = test_state_store("ctrl-k");
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_command_palette_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            frame_area,
        );

        assert_eq!(
            app.prompt_input().unwrap().as_str(),
            "focus",
            "ctrl-k must not delete anything"
        );
        let Some(Prompt::CommandPalette(prompt)) = &app.prompt else {
            panic!("palette is open");
        };
        let len = app.active_command_palette_entries().len();
        assert_eq!(
            prompt.selected.index(),
            len - 1,
            "ctrl-k wraps to the last entry"
        );

        // ...and it means the same nothing-destructive thing in a prompt with
        // no list at all.
        let mut app = App::default();
        assert!(app.begin_search());
        for ch in "needle".chars() {
            app.push_prompt_char(ch);
        }
        handle_search_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.prompt_input().unwrap().as_str(), "needle");
    }

    // ---- E4: the help overlay ---------------------------------------------

    #[test]
    fn f1_opens_help_over_a_selected_pty_but_a_bare_question_mark_does_not() {
        let store = test_state_store("help-f1");
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();

        // The seed state has a terminal selected, so `?` belongs to it.
        assert!(app.pty_input_target().is_some());
        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            frame_area,
        );
        assert!(!app.is_help_visible(), "? must reach a pane that wants it");

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
            frame_area,
        );
        assert!(app.is_help_visible());
    }

    #[test]
    fn the_help_overlay_swallows_keys_and_closes_on_the_next_one() {
        let store = test_state_store("help-modal");
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        app.show_help();

        // A key aimed at the overlay must not start or type at a PTY behind it.
        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            frame_area,
        );
        assert!(!app.is_help_visible());
        assert!(!pty_runtime.is_running(app.pty_input_target().expect("a pane is selected")));

        // Quit still works from the overlay: it is checked before the overlay.
        app.show_help();
        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(app.should_quit);
    }

    #[test]
    fn the_palette_can_open_the_overlay_and_ask_for_a_config_reload() {
        let store = test_state_store("help-palette");
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();

        execute_command_action(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            CommandAction::ShowKeybindings,
            frame_area,
        );
        assert!(app.is_help_visible());

        execute_command_action(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            CommandAction::ReloadConfig,
            frame_area,
        );
        // The action only records the request; the loop, which owns the
        // `Config`, performs the swap (E9).
        assert!(app.take_config_reload_request());
    }

    // ---- E2 / B8: connection failures reach the user ----------------------

    #[test]
    fn a_connection_wide_failure_is_reported_on_the_status_surface() {
        // A daemon that will not connect used to queue its (good) diagnostic
        // against `PtyKey::Terminal(TerminalId(0))`, a pane that cannot exist,
        // so the user saw an inert UI and no explanation at all.
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();

        apply_pty_event(
            &mut app,
            &mut pty_runtime,
            PtyEvent::ConnectionError {
                message: "protocol version 9 is not supported".to_string(),
            },
        );

        assert_eq!(app.notices().len(), 1);
        assert_eq!(app.notices()[0].level(), NoticeLevel::Error);
        assert!(app.notices()[0].text().contains("protocol version 9"));
    }

    #[test]
    fn ctrl_n_dismisses_notices_but_otherwise_reaches_the_pty() {
        let store = test_state_store("notice-dismiss");
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        app.push_notice(
            NoticeLevel::Error,
            NoticeSource::Report,
            "daemon unreachable",
        );

        assert!(handle_control_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            frame_area,
        ));
        assert!(app.notices().is_empty());

        // With nothing to dismiss the key is not consumed, so a shell behind
        // the surface keeps its `Ctrl+n`.
        assert!(!handle_control_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            frame_area,
        ));
    }

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
