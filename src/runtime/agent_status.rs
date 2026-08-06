//! The agent status bridge: reading per-chat status journals the agents append
//! to, and reconciling them against the daemon, which stays authoritative.

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use mult_protocol::{
    peer::effective_uid, AgentGeneration as WireAgentGeneration, AgentKind as WireAgentKind,
    AgentSessionMetadata, AgentStatus, AgentStatusQuery, AgentStatusRecord,
    AGENT_STATUS_SCHEMA_VERSION,
};
use serde::Deserialize;

use crate::{
    app::App,
    model::{self, AgentKind, ChatStatus, PtyKey},
    pty::PtyRuntime,
};

use super::mult_runtime_dir;

/// The environment the launched agent reads its journal contract from: where
/// to append, which schema, and which session and generation the records
/// belong to. Set by the launch path, validated by [`status_record_matches`].
pub(super) const MULT_AGENT_STATUS_PATH_ENV: &str = "MULT_AGENT_STATUS_PATH";
pub(super) const MULT_AGENT_CHAT_ID_ENV: &str = "MULT_AGENT_CHAT_ID";
pub(super) const MULT_AGENT_STATUS_VERSION_ENV: &str = "MULT_AGENT_STATUS_VERSION";
pub(super) const MULT_AGENT_NAMESPACE_ENV: &str = "MULT_AGENT_NAMESPACE";
pub(super) const MULT_AGENT_SESSION_TOKEN_ENV: &str = "MULT_AGENT_SESSION_TOKEN";
pub(super) const MULT_AGENT_KIND_ENV: &str = "MULT_AGENT_KIND";
pub(super) const MULT_AGENT_GENERATION_ENV: &str = "MULT_AGENT_GENERATION";

/// How often the per-chat agent status journals are read (S3/B3).
const AGENT_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MultAgentStatusRecord {
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
pub(super) trait AgentStatusSource {
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
pub(super) struct AgentStatusBridge<S> {
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
pub(super) struct JournalStatusSource {
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

pub(super) fn agent_session_metadata(
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

pub(super) fn reconcile_agent_status(
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

pub(super) fn drain_mult_agent_status_events(
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

pub(super) fn agent_status_kind(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Pi => "pi",
        AgentKind::ClaudeCode => "claude_code",
    }
}

/// The agent backend a chat runs, looked up by chat id alone (the durable model
/// keys chats under workspaces, but PTY events only carry the chat id).
pub(super) fn chat_agent_kind(app: &App, chat_id: model::ChatId) -> AgentKind {
    app.project
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.chats.iter())
        .find(|chat| chat.id == chat_id)
        .map(|chat| chat.agent)
        .unwrap_or_default()
}

pub(super) fn mult_agent_status_path(
    identity: model::SessionIdentity,
    generation: model::AgentGeneration,
) -> PathBuf {
    mult_runtime_dir().join("status-v1").join(format!(
        "{}-{}-{}.jsonl",
        identity.namespace, identity.token, generation
    ))
}

pub(super) fn prepare_mult_agent_status_file(path: &Path) -> io::Result<()> {
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

#[cfg(test)]
mod tests {

    use super::*;
    use crate::runtime::{agent_command::MULT_CLAUDE_STATUS_SCRIPT_SOURCE, test_support::*};

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
}
