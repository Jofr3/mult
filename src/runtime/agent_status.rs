//! Where a chat's live status comes from: one 0600 JSON file per chat, written
//! by the agent's own status hook and re-read on a timer.
//!
//! The source is behind a trait so what the loop *does* with a status can be
//! tested without a filesystem, a private runtime directory, or the JSON
//! encoding (F10).

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::{
    app::App,
    model::{self, ChatStatus, PtyKey},
    pty::PtyRuntime,
};

use super::agent_launch::{ensure_mult_runtime_dir, mult_runtime_dir};

/// The environment variable the spawned agent finds its status file under.
pub(super) const MULT_AGENT_STATUS_PATH_ENV: &str = "MULT_AGENT_STATUS_PATH";
/// The chat id the spawned agent reports under, for hooks that log it.
pub(super) const MULT_AGENT_CHAT_ID_ENV: &str = "MULT_AGENT_CHAT_ID";
/// How often the per-chat agent status files are re-read. They are written by
/// human-paced agent events, so anything faster is pure syscall traffic.
const AGENT_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct MultAgentStatusRecord {
    status: String,
}

/// Per-chat agent status-file polling.
///
/// Every tick used to rebuild each chat's status path, and every rebuild ran
/// `ensure_private_dir` (a `mkdir` plus an `lstat` per ancestor) before opening
/// and parsing the file — roughly 750 syscalls a second at idle with six chats.
/// The runtime directory is now resolved once per process, each chat's path is
/// built once, and the files are read on a timer.
/// Where a chat's agent status comes from.
///
/// A seam, not an abstraction for its own sake: the only production source is a
/// file on disk, and until this existed the only way to test what the loop did
/// with a status was to write real files into a temp directory and hope the
/// runtime directory resolved (F10). The file-backed implementation keeps its
/// own path cache, because caching is a property of *that* source and of no
/// other.
pub(super) trait AgentStatusSource {
    /// The status this chat's agent currently reports, or `None` when it
    /// reports nothing readable.
    fn status(&mut self, chat: model::ChatId) -> Option<ChatStatus>;
}

/// The production source: one 0600 JSON file per chat, written by the agent's
/// own status hook.
///
/// Every tick used to rebuild each chat's path, and every rebuild ran
/// `ensure_private_dir` (a `mkdir` plus an `lstat` per ancestor) before opening
/// and parsing the file — roughly 750 syscalls a second at idle with six chats.
/// The runtime directory is resolved once per process and each chat's path is
/// built once.
#[derive(Debug)]
pub(super) struct FileAgentStatusSource {
    /// Resolved once, at construction: the directory cannot change under a
    /// running process, and taking it as a field is also what lets a test point
    /// the source at a directory it made itself instead of at whatever the
    /// environment happens to offer (G15).
    dir: Option<PathBuf>,
    paths: HashMap<model::ChatId, PathBuf>,
}

impl FileAgentStatusSource {
    /// The production source, rooted at this process's runtime directory.
    pub(super) fn new() -> Self {
        Self::in_dir(mult_agent_status_dir().map(Path::to_path_buf))
    }

    fn in_dir(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            paths: HashMap::new(),
        }
    }

    /// The chat's status file, or `None` when there is no private directory to
    /// hold one (see [`mult_agent_status_dir`]). Nothing is cached in that case,
    /// so the decision stays with the one place that makes it.
    fn path(&mut self, chat: model::ChatId) -> Option<&Path> {
        if !self.paths.contains_key(&chat) {
            let path = agent_status_file(self.dir.as_deref()?, chat);
            self.paths.insert(chat, path);
        }
        self.paths.get(&chat).map(PathBuf::as_path)
    }
}

impl AgentStatusSource for FileAgentStatusSource {
    fn status(&mut self, chat: model::ChatId) -> Option<ChatStatus> {
        let path = self.path(chat)?;
        read_mult_agent_status(path)
    }
}

/// In-memory test double. What a chat reports is set directly, so a test of the
/// loop's behaviour does not also depend on a filesystem, a private runtime
/// directory, or the JSON encoding.
#[cfg(test)]
#[derive(Debug, Default)]
struct MapAgentStatusSource {
    statuses: HashMap<model::ChatId, ChatStatus>,
    /// Every chat that was asked, in order, so a test can assert which chats
    /// were *not* consulted — the `is_live` gate is exactly that claim.
    asked: Vec<model::ChatId>,
}

#[cfg(test)]
impl MapAgentStatusSource {
    fn with(chat: model::ChatId, status: ChatStatus) -> Self {
        let mut source = Self::default();
        source.statuses.insert(chat, status);
        source
    }
}

#[cfg(test)]
impl AgentStatusSource for MapAgentStatusSource {
    fn status(&mut self, chat: model::ChatId) -> Option<ChatStatus> {
        self.asked.push(chat);
        self.statuses.get(&chat).copied()
    }
}

/// Reads each chat's agent status on a timer.
///
/// They are written by human-paced agent events, so anything faster than
/// [`AGENT_STATUS_POLL_INTERVAL`] is pure syscall traffic.
#[derive(Debug, Default)]
pub(super) struct AgentStatusPoller<S> {
    source: S,
    next_poll: Option<Instant>,
}

impl<S: AgentStatusSource> AgentStatusPoller<S> {
    pub(super) fn new(source: S) -> Self {
        Self {
            source,
            next_poll: None,
        }
    }

    /// Whether a poll is due at `now`, arming the next one if so.
    fn due(&mut self, now: Instant) -> bool {
        match self.next_poll {
            Some(next) if now < next => false,
            _ => {
                self.next_poll = Some(now + AGENT_STATUS_POLL_INTERVAL);
                true
            }
        }
    }
}

pub(super) fn drain_mult_agent_status_events(
    app: &mut App,
    pty_runtime: &PtyRuntime,
    poller: &mut AgentStatusPoller<impl AgentStatusSource>,
    now: Instant,
) -> bool {
    if !poller.due(now) {
        return false;
    }

    apply_agent_statuses(app, &mut poller.source, |chat| {
        pty_runtime.is_running(PtyKey::ChatAgent(chat))
    })
}

/// Apply each live chat's reported status to its chat status.
///
/// `is_live` decides whether a chat's file still speaks for it. Only the agent
/// process writes that file, so once its PTY is gone the file is a leftover:
/// re-reading it undid the `Done`/`Failed` the exit event had just set, pinning
/// a finished chat at "thinking" for the rest of the session. The file is also
/// deleted on exit; this gate covers the window where a hook writes it one last
/// time after the process is gone.
fn apply_agent_statuses(
    app: &mut App,
    source: &mut impl AgentStatusSource,
    is_live: impl Fn(model::ChatId) -> bool,
) -> bool {
    let chats = app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id))
        .collect::<Vec<_>>();

    let mut changed = false;
    for chat in chats {
        if !is_live(chat) {
            continue;
        }
        if let Some(status) = source.status(chat) {
            changed |= app.mark_chat_status_by_id(chat, status);
        }
    }
    changed
}

/// Upper bound on the agent status file. It is a tiny JSON object; anything
/// larger is a bug or a hostile same-UID writer, and this read happens on the
/// render thread once per frame per chat, so it must never read unboundedly.
const MAX_STATUS_FILE_BYTES: u64 = 64 * 1024;

fn read_mult_agent_status(path: &Path) -> Option<ChatStatus> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW: never follow a symlink swapped in for the status file.
        // O_NONBLOCK: opening a FIFO or device must not stall the render thread.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }

    let file = options.open(path).ok()?;
    // Read regular files only; a swapped-in FIFO/socket/device is ignored.
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }

    let mut contents = String::new();
    file.take(MAX_STATUS_FILE_BYTES)
        .read_to_string(&mut contents)
        .ok()?;
    let record = serde_json::from_str::<MultAgentStatusRecord>(&contents).ok()?;
    mult_agent_status_to_chat_status(&record.status)
}

fn mult_agent_status_to_chat_status(status: &str) -> Option<ChatStatus> {
    match status {
        "idle" => Some(ChatStatus::Idle),
        "running" => Some(ChatStatus::Thinking),
        "waiting" => Some(ChatStatus::Waiting),
        "error" => Some(ChatStatus::Failed),
        "finished" => Some(ChatStatus::Done { seen: false }),
        _ => None,
    }
}

/// Remove the per-chat status files this process created.
///
/// Unlike the generated scripts these cannot be content-addressed — the name
/// carries the pid so two `mult` instances do not fight over one file — so they
/// are cleaned up explicitly on the way out. A missed file (a crash, a kill -9)
/// is harmless: it is a 0600 JSON document, and a restarting `mult` truncates
/// the file for a chat before handing its path to a new agent.
pub(super) fn remove_agent_status_files(app: &App) {
    for chat in app
        .project
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.chats.iter().map(|chat| chat.id))
    {
        if let Some(path) = mult_agent_status_path(chat) {
            let _ = fs::remove_file(path);
        }
    }
}

/// The per-chat status file, or `None` when there is no private directory to
/// put it in. See [`mult_agent_status_dir`].
pub(super) fn mult_agent_status_path(chat: model::ChatId) -> Option<PathBuf> {
    Some(agent_status_file(mult_agent_status_dir()?, chat))
}

/// A chat's status file inside `dir`. The pid is in the name so two `mult`
/// instances do not fight over one file.
fn agent_status_file(dir: &Path, chat: model::ChatId) -> PathBuf {
    dir.join(format!(
        "mult-agent-status-{}-{}.json",
        std::process::id(),
        chat.get()
    ))
}

/// The directory the per-chat status files live in, resolved once per process,
/// or `None` when it is not private.
///
/// `ensure_mult_runtime_dir` costs a `mkdir` plus an `lstat` per ancestor, and
/// this used to run for every chat on every frame. The directory cannot change
/// under a running process (it is derived from `XDG_RUNTIME_DIR` and the euid),
/// so one resolution is enough.
///
/// The resolution fails closed. It used to fall back to `mult_runtime_dir()` —
/// the *exact* path `ensure_private_dir` had just rejected for being owned by
/// someone else or writable by group/others — and then exported it to the agent
/// as `$MULT_AGENT_STATUS_PATH` and called `remove_file` inside it. Losing the
/// status dot is a cosmetic degradation; handing an attacker-controlled
/// directory to a spawned agent is not, so a rejected directory now yields no
/// status reporting at all and the caller reports why.
fn mult_agent_status_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| ensure_mult_runtime_dir().ok())
        .as_deref()
}

/// Why status reporting is unavailable, for the message shown to the user.
pub(super) fn mult_agent_status_dir_error() -> Option<io::Error> {
    if mult_agent_status_dir().is_some() {
        return None;
    }
    // Re-run the check purely to recover the diagnostic; the cache above holds
    // only the decision, and this path is taken once per agent start at most.
    ensure_mult_runtime_dir().err().or_else(|| {
        Some(io::Error::other(format!(
            "{} is not a private directory",
            mult_runtime_dir().display()
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        super::agent_launch::{private_test_dir, MULT_CLAUDE_STATUS_SCRIPT_SOURCE},
        *,
    };
    use crate::model::{AgentKind, ProjectState};

    #[test]
    fn read_mult_agent_status_parses_a_small_status_file() {
        let path = unique_status_path("small");
        fs::write(&path, r#"{"status":"running"}"#).expect("write status");

        assert_eq!(read_mult_agent_status(&path), Some(ChatStatus::Thinking));

        let _ = fs::remove_file(&path);
    }
    #[test]
    fn read_mult_agent_status_caps_the_read_and_rejects_oversized_files() {
        let path = unique_status_path("huge");
        // Valid JSON as a whole, but far larger than the cap. Read in full it
        // would parse; truncated at the cap it cannot, so a bounded read rejects
        // it — proving the read never grows with the file.
        let padding = " ".repeat(MAX_STATUS_FILE_BYTES as usize + 1024);
        fs::write(&path, format!(r#"{{"status":"idle",{padding}"x":1}}"#)).expect("write status");

        assert_eq!(read_mult_agent_status(&path), None);

        let _ = fs::remove_file(&path);
    }
    #[cfg(unix)]
    #[test]
    fn read_mult_agent_status_does_not_follow_symlinks() {
        let target = unique_status_path("symlink-target");
        fs::write(&target, r#"{"status":"idle"}"#).expect("write status");
        let link = unique_status_path("symlink-link");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        // O_NOFOLLOW means the symlink is not traversed, so nothing is read.
        assert_eq!(read_mult_agent_status(&link), None);

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
    // The two halves of the feature must agree on the file schema: the bundled
    // shell script has to write exactly what `read_mult_agent_status` parses.
    #[cfg(unix)]
    #[test]
    fn bundled_claude_status_script_writes_a_status_mult_can_read() {
        let script = unique_status_path("cc-script");
        fs::write(&script, MULT_CLAUDE_STATUS_SCRIPT_SOURCE).expect("write script");
        let status_path = unique_status_path("cc-status");
        let _ = fs::remove_file(&status_path);

        let output = std::process::Command::new("sh")
            .arg(&script)
            .arg("running")
            .env(MULT_AGENT_STATUS_PATH_ENV, &status_path)
            .env(MULT_AGENT_CHAT_ID_ENV, "7")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run status script");
        assert!(output.status.success());

        // `running` round-trips through the file into mult's Thinking status.
        assert_eq!(
            read_mult_agent_status(&status_path),
            Some(ChatStatus::Thinking)
        );

        let _ = fs::remove_file(&script);
        let _ = fs::remove_file(&status_path);
    }
    #[test]
    fn a_reported_agent_status_updates_the_chat_status() {
        // Reported through the seam, not through a file: what this asserts is
        // what the loop does with a status, not how one is spelled on disk
        // (F10). The file-backed source is covered separately, below.
        let mut app = app_with_one_chat();
        let chat = app.project.workspaces[0].chats[0].id;
        let mut source = MapAgentStatusSource::with(chat, ChatStatus::Done { seen: false });

        assert!(apply_agent_statuses(&mut app, &mut source, |_| true));

        // The chat is the selected item, so finishing counts as seen at once.
        assert_eq!(
            app.project.workspaces[0].chats[0].status,
            ChatStatus::Done { seen: true }
        );
    }
    #[test]
    fn a_stale_status_does_not_resurrect_a_finished_chat() {
        // The agent exited and `drain_pty_events` marked the chat `Done`, but
        // its last status report ("running") is still there. Re-reading it used
        // to flip the chat back to `Thinking` on the very next frame — and every
        // frame after that, forever. The gate is that a chat whose PTY is gone
        // is not even asked.
        let mut app = app_with_one_chat();
        let chat = app.project.workspaces[0].chats[0].id;
        app.mark_chat_status_by_id(chat, ChatStatus::Done { seen: false });
        let mut source = MapAgentStatusSource::with(chat, ChatStatus::Thinking);

        let changed = apply_agent_statuses(&mut app, &mut source, |_| false);

        assert!(!changed);
        assert!(source.asked.is_empty(), "a dead chat must not be consulted");
        assert_eq!(
            app.project.workspaces[0].chats[0].status,
            ChatStatus::Done { seen: true }
        );
    }
    /// A chat whose source reports nothing keeps whatever status it had.
    #[test]
    fn a_chat_with_no_reported_status_is_left_alone() {
        let mut app = app_with_one_chat();
        let chat = app.project.workspaces[0].chats[0].id;
        app.mark_chat_status_by_id(chat, ChatStatus::Waiting);
        let mut source = MapAgentStatusSource::default();

        assert!(!apply_agent_statuses(&mut app, &mut source, |_| true));

        assert_eq!(source.asked, vec![chat]);
        assert_eq!(
            app.project.workspaces[0].chats[0].status,
            ChatStatus::Waiting
        );
    }
    fn app_with_one_chat() -> App {
        let mut state = ProjectState::two_workspaces();
        let workspace = state.workspaces[0].id;
        state.add_chat(
            workspace,
            model::DEFAULT_AGENT_CHAT_TITLE.to_string(),
            ChatStatus::Idle,
            AgentKind::Pi,
        );
        App::new(state)
    }
    #[test]
    fn agent_status_polling_is_rate_limited() {
        let mut poller = AgentStatusPoller::new(MapAgentStatusSource::default());
        let start = Instant::now();

        // The first poll always runs, the next one only after the interval.
        assert!(poller.due(start));
        assert!(!poller.due(start));
        assert!(!poller.due(start + AGENT_STATUS_POLL_INTERVAL - Duration::from_millis(1)));
        assert!(poller.due(start + AGENT_STATUS_POLL_INTERVAL));
        assert!(!poller.due(start + AGENT_STATUS_POLL_INTERVAL));
    }
    #[test]
    fn agent_status_paths_are_cached_per_chat() {
        // Rooted at the test's own private directory rather than at the
        // process's, so what is asserted is the caching, not whether the
        // ambient filesystem happens to pass the privacy check (G15).
        let dir = private_test_dir("status-paths");
        let mut source = FileAgentStatusSource::in_dir(Some(dir.clone()));
        let chat = model::ChatId::new(4_242).unwrap();

        let path = source.path(chat).expect("status path in dir").to_path_buf();

        assert_eq!(source.path(chat), Some(path.as_path()));
        assert_eq!(path, agent_status_file(&dir, chat));
        assert_eq!(source.paths.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }
    /// Without a private directory there is no status file, and nothing is
    /// cached — the decision stays with `mult_agent_status_dir`.
    #[test]
    fn no_private_directory_means_no_status_path() {
        let mut source = FileAgentStatusSource::in_dir(None);
        let chat = model::ChatId::new(7).unwrap();

        assert_eq!(source.path(chat), None);
        assert!(source.paths.is_empty());
    }
}
