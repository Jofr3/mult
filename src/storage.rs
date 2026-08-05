use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use mult_protocol::rand::random_u64;

use crate::model::{ProjectState, STATE_VERSION};

const STATE_PATH_ENV: &str = "MULT_STATE_PATH";

/// What went wrong reading or writing the project state.
///
/// Persistence used to answer in `io::Error`, so "the JSON is malformed", "the
/// file is newer than this build", "the backup rename failed" and "the disk is
/// full" all arrived as the same type and were told apart, if at all, by their
/// rendered text (F8). Each is now its own variant, and each carries the path it
/// is about — the thing the message most often failed to say.
#[derive(Debug)]
pub enum StateError {
    /// The file could not be read or written.
    Io { path: PathBuf, source: io::Error },
    /// The bytes were read but are not a project state.
    Decode {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// The file was written by a newer build. Nothing is modified: a downgrade
    /// must not quietly reinterpret, or discard, a format it does not know.
    UnsupportedVersion {
        path: PathBuf,
        version: u32,
        supported: u32,
    },
    /// The state could not be serialized. Not path-scoped: nothing was written.
    Encode(serde_json::Error),
    /// A damaged file could not be moved aside. Reported rather than worked
    /// around, because resetting anyway would destroy the only copy.
    BackupFailed {
        path: PathBuf,
        backup: PathBuf,
        decode_error: serde_json::Error,
        source: io::Error,
    },
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Decode { path, source } => {
                write!(f, "{} is not valid state JSON: {source}", path.display())
            }
            Self::UnsupportedVersion {
                path,
                version,
                supported,
            } => write!(
                f,
                "state file version {version} is newer than supported version {supported}; not modifying {}",
                path.display()
            ),
            Self::Encode(source) => write!(f, "cannot encode project state: {source}"),
            Self::BackupFailed {
                path,
                backup,
                decode_error,
                source,
            } => write!(
                f,
                "state JSON is invalid ({decode_error}); failed to move {} to {}: {source}",
                path.display(),
                backup.display()
            ),
        }
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::BackupFailed { source, .. } => Some(source),
            Self::Decode { source, .. } | Self::Encode(source) => Some(source),
            Self::UnsupportedVersion { .. } => None,
        }
    }
}

/// The boundary conversion, for the callers that still report `io::Result`.
impl From<StateError> for io::Error {
    fn from(error: StateError) -> Self {
        let kind = match &error {
            StateError::Io { source, .. } => source.kind(),
            StateError::Decode { .. }
            | StateError::UnsupportedVersion { .. }
            | StateError::Encode(_)
            | StateError::BackupFailed { .. } => io::ErrorKind::InvalidData,
        };
        io::Error::new(kind, error.to_string())
    }
}

/// Where the project state lives, as a thing that can be handed to the event
/// loop rather than a global the loop reaches for.
///
/// Persistence used to be a free function deriving its path from the process
/// environment at call time, so nothing could observe a save without mutating
/// the environment of the whole test binary, and the testable core
/// (`save_to_path`/`load_from_path`) was private (F11). The loop now holds a
/// store; production passes [`FileStateStore`] and tests pass
/// [`MemoryStateStore`].
pub trait StateStore {
    fn load(&self) -> Result<LoadedState, StateError>;
    fn save(&mut self, state: &ProjectState) -> Result<(), StateError>;
}

/// The real store: one `state.json`, written atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStateStore {
    path: PathBuf,
}

impl FileStateStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl StateStore for FileStateStore {
    fn load(&self) -> Result<LoadedState, StateError> {
        load_from_path(&self.path)
    }

    fn save(&mut self, state: &ProjectState) -> Result<(), StateError> {
        save_to_path(state, &self.path)
    }
}

/// An in-memory store, for tests that need to observe *whether and what* was
/// saved without touching a filesystem or the process environment.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemoryStateStore {
    state: Option<ProjectState>,
    saves: usize,
}

impl MemoryStateStore {
    /// A store that already holds `state`, as if a previous session saved it.
    pub fn with_state(state: ProjectState) -> Self {
        Self {
            state: Some(state),
            saves: 0,
        }
    }

    /// The most recently saved state, or `None` if nothing was ever saved.
    pub fn saved(&self) -> Option<&ProjectState> {
        self.state.as_ref()
    }

    /// How many times `save` has been called. The rate limiting in the event
    /// loop is a claim about this number, so the number has to be visible.
    pub fn save_count(&self) -> usize {
        self.saves
    }
}

impl StateStore for MemoryStateStore {
    fn load(&self) -> Result<LoadedState, StateError> {
        Ok(match &self.state {
            Some(state) => LoadedState::intact(state.clone()),
            None => LoadedState {
                state: ProjectState::default(),
                notice: None,
                first_run: true,
            },
        })
    }

    fn save(&mut self, state: &ProjectState) -> Result<(), StateError> {
        self.state = Some(state.clone());
        self.saves += 1;
        Ok(())
    }
}

/// A loaded project state, plus anything the user needs to be told about how it
/// was loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedState {
    pub state: ProjectState,
    /// Set when the load is worth telling the user about: the file could not be
    /// decoded and was moved aside, or it carried data this build drops. It
    /// names the file it produced, because the alternative — silently presenting
    /// something different from what is on disk — is indistinguishable from
    /// having lost it (E11).
    pub notice: Option<String>,
    /// Whether there was no state file at all, i.e. this is a first run.
    ///
    /// Only this case may be seeded with starter content. A file that existed
    /// but could not be read is *not* a first run, however empty the recovered
    /// state is — handing that user a fabricated workspace is exactly what F12
    /// removed.
    pub first_run: bool,
}

impl LoadedState {
    fn intact(state: ProjectState) -> Self {
        Self {
            state,
            notice: None,
            first_run: false,
        }
    }
}

/// The state path in force: `--state`, else `$MULT_STATE_PATH`, else the XDG
/// default. Resolved once by `main` and handed to a [`FileStateStore`]; there is
/// no process-global override any more (F11).
pub fn state_path_with_override(flag: Option<PathBuf>) -> PathBuf {
    state_path_from(
        flag.as_deref(),
        env::var_os(STATE_PATH_ENV).as_deref(),
        &data_home(),
    )
}

/// Pure core of [`state_path_with_override`]: both the flag and the environment
/// arrive as arguments so the precedence rule can be tested without mutating a
/// process global. `--state` beats `$MULT_STATE_PATH`, which beats the XDG
/// default.
fn state_path_from(
    flag: Option<&Path>,
    override_path: Option<&OsStr>,
    data_home: &Path,
) -> PathBuf {
    match (flag, override_path) {
        (Some(path), _) => path.to_path_buf(),
        (None, Some(path)) => PathBuf::from(path),
        (None, None) => data_home.join("mult").join("state.json"),
    }
}

/// Upper bound on `state.json`. A real state file is a few kilobytes of
/// workspace, chat and terminal metadata; the cap exists so a FIFO or a
/// `/dev/zero` symlink at an inherited `$MULT_STATE_PATH` cannot make startup
/// allocate without limit.
const MAX_STATE_BYTES: u64 = 32 * 1024 * 1024;

/// Read the state file, then decode it.
///
/// `state.json` is an execution channel — `restore_persisted_sessions` replays
/// every terminal marked for restore — and its path is
/// `$MULT_STATE_PATH`-overridable, so the read itself is guarded:
/// [`mult_protocol::read_private_file`] refuses a symlink, a non-regular file,
/// a file owned by someone else, one anyone else can write, and one over the
/// cap. A missing file still means "first run"; a *refused* file is an error,
/// not an empty project, so nothing silently resets state that is merely
/// unreadable. Only a successfully-read-but-undecodable file is backed up.
fn load_from_path(path: &Path) -> Result<LoadedState, StateError> {
    let bytes = match mult_protocol::read_private_file(path, MAX_STATE_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadedState {
                state: ProjectState::default(),
                notice: None,
                first_run: true,
            })
        }
        Err(source) => {
            return Err(StateError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    let mut state = match decode_project_state(&bytes) {
        Ok(state) => state,
        Err(StateDecodeError::InvalidJson(error)) => {
            return backup_invalid_state_and_reset(path, error)
        }
        Err(StateDecodeError::UnsupportedVersion(version)) => {
            return Err(StateError::UnsupportedVersion {
                path: path.to_path_buf(),
                version,
                supported: STATE_VERSION,
            })
        }
    };

    // Renames first: they lose nothing, so there is nothing to preserve.
    state.migrate_legacy_terminal_status();
    // Then the migrations that *drop* data, before anything can save over the
    // file, so the copy that is preserved is the one the user still has.
    let notice = preserve_dropped_fields(path, &bytes, &mut state);
    Ok(LoadedState {
        state,
        notice,
        first_run: false,
    })
}

/// Copy the file aside when the decode dropped something this build no longer
/// keeps, and say where the copy went.
///
/// Only pre-11a chat transcripts qualify today: F1 deleted the transcript path
/// after establishing that nothing in production ever wrote to it, so the field
/// is read (a state file carrying one still loads intact) and then discarded.
/// Discarding it silently would be indistinguishable from losing it, since the
/// next save rewrites the file without it — hence a *copy*, not a rename: the
/// state itself is fine and keeps loading from where it always did.
///
/// A copy that cannot be written is not fatal. Failing the load over it would
/// deny the user their whole project to protect a transcript they could never
/// see; the notice says so instead.
fn preserve_dropped_fields(path: &Path, bytes: &[u8], state: &mut ProjectState) -> Option<String> {
    let dropped = state.take_legacy_chat_transcripts();
    if dropped == 0 {
        return None;
    }

    let messages = if dropped == 1 {
        "1 chat message".to_string()
    } else {
        format!("{dropped} chat messages")
    };
    match migration_backup_path(path).and_then(|backup| {
        write_private_copy(&backup, bytes)?;
        Ok(backup)
    }) {
        Ok(backup) => Some(format!(
            "{} held {messages} from a removed chat-transcript feature; they are no longer shown. A copy of the file was saved to {}",
            path.display(),
            backup.display()
        )),
        Err(error) => Some(format!(
            "{} held {messages} from a removed chat-transcript feature; they are no longer shown, and the copy could not be written ({error})",
            path.display()
        )),
    }
}

fn decode_project_state(bytes: &[u8]) -> Result<ProjectState, StateDecodeError> {
    let mut state: ProjectState =
        serde_json::from_slice(bytes).map_err(StateDecodeError::InvalidJson)?;
    if state.version > STATE_VERSION {
        return Err(StateDecodeError::UnsupportedVersion(state.version));
    }
    if state.version < STATE_VERSION {
        state.version = STATE_VERSION;
    }
    Ok(state)
}

#[derive(Debug)]
enum StateDecodeError {
    InvalidJson(serde_json::Error),
    UnsupportedVersion(u32),
}

fn save_to_path(state: &ProjectState, path: &Path) -> Result<(), StateError> {
    ensure_parent_dir(path).map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let json = serde_json::to_string_pretty(state).map_err(StateError::Encode)?;
    write_atomically(path, format!("{json}\n").as_bytes()).map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn backup_invalid_state_and_reset(
    path: &Path,
    decode_error: serde_json::Error,
) -> Result<LoadedState, StateError> {
    let backup = corrupt_backup_path(path).map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    fs::rename(path, &backup).map_err(|source| StateError::BackupFailed {
        path: path.to_path_buf(),
        backup: backup.clone(),
        decode_error: clone_decode_error(&decode_error),
        source,
    })?;
    // The reset itself is not the news — the backup is. Without this the user
    // is shown an empty project and has no way to know their workspaces are
    // still on disk under a name they never chose (E11).
    Ok(LoadedState {
        state: ProjectState::default(),
        first_run: false,
        notice: Some(format!(
            "{} could not be read ({decode_error}); starting empty. Your previous state was saved to {}",
            path.display(),
            backup.display()
        )),
    })
}

/// `serde_json::Error` is not `Clone`, and the backup path needs the message in
/// two places (the error variant and, on success, the notice). Re-rendering it
/// as a JSON syntax error keeps the wording the user sees identical.
fn clone_decode_error(error: &serde_json::Error) -> serde_json::Error {
    serde::de::Error::custom(error.to_string())
}

fn corrupt_backup_path(path: &Path) -> io::Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    // A random suffix keeps the name unique and unpredictable without an
    // exists()-then-rename probe (a TOCTOU pattern); the rename onto it is
    // atomic, mirroring how create_temp_state_file already picks its path.
    let random = random_u64()?;
    Ok(path.with_extension(format!("json.corrupt-{timestamp}-{random:016x}")))
}

/// Where a pre-migration copy of the state file goes. Same naming rule as
/// [`corrupt_backup_path`] — timestamp plus random suffix, so the name is unique
/// without an `exists()`-then-write probe — under a suffix that says what it is.
fn migration_backup_path(path: &Path) -> io::Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let random = random_u64()?;
    Ok(path.with_extension(format!("json.pre-11a-{timestamp}-{random:016x}")))
}

/// Write `contents` to a path that must not already exist, with the same
/// user-only mode the state file itself gets.
fn write_private_copy(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let (temp_path, mut file) = create_temp_state_file(path)?;
    let result = (|| {
        restrict_state_file_permissions(&file)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        sync_parent_dir(path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }

    result
}

fn create_temp_state_file(path: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..64 {
        let temp_path = random_temp_save_path(path)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }

        match options.open(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create unique temporary state file",
    ))
}

fn random_temp_save_path(path: &Path) -> io::Result<PathBuf> {
    let mut file_name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("state.json"));
    file_name.push(format!(
        ".tmp-{}-{:016x}",
        std::process::id(),
        random_u64()?
    ));
    Ok(path.with_file_name(file_name))
}

fn restrict_state_file_permissions(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = file.metadata()?.permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)?;
    }

    Ok(())
}

fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_private_dir_all(parent)?;
    }
    Ok(())
}

fn sync_parent_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn create_private_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

#[cfg(test)]
fn legacy_temp_save_path(path: &Path, attempt: usize) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("state.json"));
    file_name.push(format!(".tmp-{}-{attempt}", std::process::id()));
    path.with_file_name(file_name)
}

fn data_home() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn state_path_prefers_the_flag_then_the_env_then_the_data_home() {
        let data_home = Path::new("/home/example/.local/share");

        assert_eq!(
            state_path_from(
                Some(Path::new("/tmp/from-flag.json")),
                Some(OsStr::new("/tmp/mult-test-state.json")),
                data_home
            ),
            PathBuf::from("/tmp/from-flag.json")
        );
        assert_eq!(
            state_path_from(
                None,
                Some(OsStr::new("/tmp/mult-test-state.json")),
                data_home
            ),
            PathBuf::from("/tmp/mult-test-state.json")
        );
        assert_eq!(
            state_path_from(None, None, data_home),
            PathBuf::from("/home/example/.local/share/mult/state.json")
        );
    }

    #[test]
    fn save_preserves_running_terminal_status_for_restart() {
        let path = unique_temp_file();
        let mut state = ProjectState::two_workspaces();
        state.workspaces[0].terminals[0].restore_on_launch = true;

        save_to_path(&state, &path).expect("save state");

        let bytes = fs::read(&path).expect("read saved state");
        let decoded: ProjectState = serde_json::from_slice(&bytes).expect("decode state");
        assert!(decoded.workspaces[0].terminals[0].restore_on_launch);
    }

    #[test]
    fn save_replaces_existing_state_without_leaving_temp_file() {
        let path = unique_temp_file();
        fs::write(&path, "old contents").expect("write old state");
        let mut state = ProjectState::two_workspaces();
        state.workspaces[0].name = "saved".to_string();

        save_to_path(&state, &path).expect("save state");

        let bytes = fs::read(&path).expect("read saved state");
        let decoded: ProjectState = serde_json::from_slice(&bytes).expect("decode state");
        assert_eq!(decoded.workspaces[0].name, "saved");
        assert_no_state_temp_files(&path);
    }

    /// F12: a damaged state file recovers to an *empty* project.
    ///
    /// It used to recover to `ProjectState::default()`, which seeded two demo
    /// workspaces — so a user whose state was corrupted was handed a `website`
    /// workspace they never created, alongside a notice saying the session was
    /// "starting empty".
    #[test]
    fn a_corrupt_state_recovers_empty_and_is_not_a_first_run() {
        let path = unique_temp_file();
        fs::write(&path, "{not json").expect("write corrupt state");

        let loaded = load_from_path(&path).expect("recover corrupt state");

        assert!(loaded.state.workspaces.is_empty());
        assert!(!loaded.first_run, "a damaged file is not a fresh install");
        assert!(loaded.notice.is_some());
        for backup in corrupt_backups_of(&path) {
            let _ = fs::remove_file(backup);
        }
    }

    /// The other half of F12: only a genuinely missing file is a first run, and
    /// the seed is the caller's business, not storage's.
    #[test]
    fn a_missing_state_file_is_a_first_run_and_storage_still_returns_nothing() {
        let path = unique_temp_file();

        let loaded = load_from_path(&path).expect("missing state is not an error");

        assert!(loaded.first_run);
        assert!(loaded.state.workspaces.is_empty());
        assert!(loaded.notice.is_none());
    }

    #[test]
    fn invalid_state_json_is_backed_up_and_reset() {
        let path = unique_temp_file();
        fs::write(&path, "{not json").expect("write corrupt state");

        let loaded = load_from_path(&path).expect("recover corrupt state");

        assert_eq!(loaded.state, ProjectState::default());
        assert!(!path.exists());
        let parent = path.parent().expect("temp path has parent");
        let stem = path.file_stem().unwrap().to_string_lossy();
        let backups = fs::read_dir(parent)
            .expect("read temp dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{stem}.json.corrupt-"))
            })
            .collect::<Vec<_>>();
        assert!(!backups.is_empty());
        // A reset the user is not told about is indistinguishable from having
        // lost everything, so the notice has to name the backup (E11).
        let notice = loaded.notice.expect("a reset must be reported");
        assert!(notice.contains(&path.display().to_string()), "{notice}");
        assert!(notice.contains(".json.corrupt-"), "{notice}");
        for backup in backups {
            let _ = fs::remove_file(backup.path());
        }
    }

    #[test]
    fn a_partially_unknown_state_keeps_everything_it_can_decode() {
        // The E11 case: one renamed key (`title` for `name`) and one `null`
        // where a list is expected. Every one of these used to fail the whole
        // decode, which moved the file aside and dropped every workspace, chat
        // and terminal. Now the file loads and only the renamed field is lost.
        let path = unique_temp_file();
        fs::write(
            &path,
            format!(
                r#"{{"version":{STATE_VERSION},"next_workspace_id":2,"next_chat_id":2,"next_terminal_id":2,
                     "workspaces":[{{"id":1,"title":"renamed","cwd":null,"environment":null,
                       "chats":[{{"id":1,"name":"agent","status":"Idle","messages":null}}],
                       "terminals":[{{"id":1,"name":"shell","status":"Running"}}]}}]}}"#
            ),
        )
        .expect("write partially unknown state");

        let loaded = load_from_path(&path).expect("lenient decode");

        assert_eq!(loaded.notice, None, "nothing was reset, so nothing to say");
        assert!(path.exists(), "a decodable file must not be moved aside");
        assert!(corrupt_backups_of(&path).is_empty());
        let workspaces = &loaded.state.workspaces;
        assert_eq!(workspaces.len(), 1);
        // The unknown key is ignored and `name` takes its default; the chat and
        // the terminal — the parts that would have been expensive to lose —
        // survive intact.
        assert_eq!(workspaces[0].name, "");
        assert!(workspaces[0].environment.is_empty());
        assert_eq!(workspaces[0].chats.len(), 1);
        assert_eq!(workspaces[0].chats[0].name, "agent");
        assert_eq!(workspaces[0].terminals.len(), 1);
        assert!(workspaces[0].terminals[0].restore_on_launch);
        let _ = fs::remove_file(&path);
    }

    /// F16: a pre-11a `status: "Running"` means "bring this back", which is
    /// exactly what `restore_on_launch` records — so an older state file
    /// restores what it always did, and the next save writes the new name.
    #[test]
    fn a_pre_11a_terminal_status_migrates_to_restore_intent() {
        let path = unique_temp_file();
        fs::write(
            &path,
            format!(
                r#"{{"version":{STATE_VERSION},"next_workspace_id":2,"next_chat_id":1,"next_terminal_id":3,
                     "workspaces":[{{"id":1,"name":"orbit","cwd":null,"environment":{{}},"chats":[],
                       "terminals":[{{"id":1,"name":"shell","status":"Running"}},
                                    {{"id":2,"name":"other","status":"Stopped"}}]}}]}}"#
            ),
        )
        .expect("write pre-11a state");

        let loaded = load_from_path(&path).expect("load pre-11a state");

        assert_eq!(loaded.notice, None, "a rename loses nothing worth saying");
        let terminals = &loaded.state.workspaces[0].terminals;
        assert_eq!(terminals.len(), 2);
        assert!(terminals[0].restore_on_launch, "Running meant restore me");
        assert!(!terminals[1].restore_on_launch);

        // The old key is gone from the next save, and the new one carries the
        // same meaning.
        save_to_path(&loaded.state, &path).expect("save migrated state");
        let saved = fs::read_to_string(&path).expect("read saved state");
        assert!(!saved.contains("\"status\""), "{saved}");
        assert!(saved.contains("\"restore_on_launch\": true"), "{saved}");

        let reloaded = load_from_path(&path).expect("reload migrated state");
        assert_eq!(reloaded.state.workspaces, loaded.state.workspaces);
        let _ = fs::remove_file(&path);
    }

    /// F16: the seen bit is a fact about this session's notifications, so it is
    /// not persisted — a finished chat comes back unseen, exactly as the old
    /// runtime-only `seen_done` set produced by starting empty. The five status
    /// strings on disk are unchanged, so no chat needs migrating at all.
    #[test]
    fn a_seen_done_chat_is_persisted_as_plain_done_and_reloads_unseen() {
        let path = unique_temp_file();
        let mut state = ProjectState::default();
        let workspace = state.add_workspace("orbit".to_string(), None);
        let chat = state
            .add_chat(
                workspace,
                "agent".to_string(),
                crate::model::ChatStatus::Done { seen: true },
                crate::model::AgentKind::Pi,
            )
            .expect("add chat");

        save_to_path(&state, &path).expect("save state");
        let saved = fs::read_to_string(&path).expect("read saved state");
        assert!(saved.contains(r#""status": "Done""#), "{saved}");
        assert!(!saved.contains("seen"), "{saved}");

        let loaded = load_from_path(&path).expect("load state");
        assert_eq!(
            loaded
                .state
                .chat(workspace, chat)
                .expect("chat survives")
                .status,
            crate::model::ChatStatus::Done { seen: false }
        );
        let _ = fs::remove_file(&path);
    }

    /// F1: a pre-11a state file carrying chat transcripts loads intact, keeps
    /// every workspace/chat/terminal, drops only the transcripts — and says so,
    /// naming a byte-for-byte copy of the file it was read from. The next save
    /// rewrites the file without the field, so the copy is the only place that
    /// data still exists.
    #[test]
    fn a_pre_11a_transcript_is_copied_aside_and_reported() {
        let path = unique_temp_file();
        let original = format!(
            r#"{{"version":{STATE_VERSION},"next_workspace_id":2,"next_chat_id":2,"next_terminal_id":2,
                 "workspaces":[{{"id":1,"name":"orbit","cwd":null,"environment":{{}},
                   "chats":[{{"id":1,"name":"agent","status":"Done","agent":"Pi",
                     "messages":[{{"role":"User","text":"hello"}},{{"role":"Assistant","text":"hi"}}]}}],
                   "terminals":[{{"id":1,"name":"shell","restore_on_launch":false}}]}}]}}"#
        );
        fs::write(&path, &original).expect("write pre-11a state");

        let loaded = load_from_path(&path).expect("a transcript must not fail the load");

        assert!(path.exists(), "the state itself is fine and stays put");
        assert_eq!(loaded.state.workspaces.len(), 1);
        assert_eq!(loaded.state.workspaces[0].chats.len(), 1);
        assert_eq!(loaded.state.workspaces[0].chats[0].name, "agent");
        assert_eq!(loaded.state.workspaces[0].terminals.len(), 1);

        let notice = loaded.notice.expect("dropping data must be reported");
        assert!(notice.contains("2 chat messages"), "{notice}");
        let copies = migration_copies_of(&path);
        assert_eq!(copies.len(), 1, "expected one copy: {copies:?}");
        assert!(
            notice.contains(&copies[0].display().to_string()),
            "{notice}"
        );
        assert_eq!(
            fs::read_to_string(&copies[0]).expect("read copy"),
            original,
            "the copy must preserve the user's bytes verbatim"
        );

        // And the next save no longer carries the field.
        save_to_path(&loaded.state, &path).expect("save migrated state");
        let saved = fs::read_to_string(&path).expect("read saved state");
        assert!(!saved.contains("messages"), "{saved}");

        for copy in copies {
            let _ = fs::remove_file(copy);
        }
        let _ = fs::remove_file(&path);
    }

    /// A state file with no transcript says nothing and copies nothing — the
    /// migration must be invisible to everyone it does not apply to.
    #[test]
    fn a_state_file_without_a_transcript_is_not_copied_or_reported() {
        let path = unique_temp_file();
        fs::write(
            &path,
            format!(
                r#"{{"version":{STATE_VERSION},"next_workspace_id":2,"next_chat_id":2,"next_terminal_id":1,
                     "workspaces":[{{"id":1,"name":"orbit","chats":[{{"id":1,"name":"agent","status":"Idle"}}],"terminals":[]}}]}}"#
            ),
        )
        .expect("write state");

        let loaded = load_from_path(&path).expect("load state");

        assert_eq!(loaded.notice, None);
        assert!(migration_copies_of(&path).is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_does_not_clobber_preexisting_temp_file() {
        let path = unique_temp_file();
        let preexisting_temp = legacy_temp_save_path(&path, 0);
        fs::write(&preexisting_temp, "do not clobber").expect("write preexisting temp file");

        save_to_path(&ProjectState::default(), &path).expect("save state");

        assert_eq!(
            fs::read_to_string(&preexisting_temp).expect("preexisting temp remains"),
            "do not clobber"
        );
        fs::remove_file(&preexisting_temp).expect("remove preexisting temp file");
    }

    #[test]
    fn future_state_versions_are_rejected_without_backup_or_reset() {
        let path = unique_temp_file();
        fs::write(
            &path,
            format!(
                r#"{{"version":{},"next_workspace_id":1,"next_chat_id":1,"next_terminal_id":1,"workspaces":[]}}"#,
                STATE_VERSION + 1
            ),
        )
        .expect("write future state");

        let error = load_from_path(&path).expect_err("future state should be rejected");

        assert!(
            matches!(error, StateError::UnsupportedVersion { version, .. } if version == STATE_VERSION + 1),
            "{error:?}"
        );
        assert!(path.exists());
        assert!(fs::read_to_string(&path)
            .expect("future state remains")
            .contains(&format!("\"version\":{}", STATE_VERSION + 1)));
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_state_file_with_user_only_permissions() {
        let path = unique_temp_file();

        save_to_path(&ProjectState::default(), &path).expect("save state");

        let mode = fs::metadata(&path)
            .expect("state metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_missing_parent_dir_with_user_only_permissions() {
        let parent = unique_temp_dir();
        let path = parent.join("nested").join("state.json");

        save_to_path(&ProjectState::default(), &path).expect("save state");

        let mode = fs::metadata(path.parent().expect("state has parent"))
            .expect("parent metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        fs::remove_dir_all(parent).expect("remove temp state dir");
    }

    #[test]
    fn parent_dir_creation_allows_bare_file_names() {
        ensure_parent_dir(Path::new("state.json"))
            .expect("bare file name has no directory to create");
    }

    #[test]
    fn structurally_invalid_state_is_backed_up_not_silently_discarded() {
        // What leniency cannot save: a whole workspace element whose id is a
        // string. Dropping just that element would lose it silently on the next
        // save, so the file is still moved aside — but the notice now says
        // where to, which is the difference between "recoverable" and "gone"
        // (E11). `workspaces: null` used to land here too and no longer does;
        // see `a_partially_unknown_state_keeps_everything_it_can_decode`.
        let path = unique_temp_file();
        let original = format!(
            r#"{{"version":{STATE_VERSION},"next_workspace_id":2,"next_chat_id":1,"next_terminal_id":1,"workspaces":[{{"id":"one"}}]}}"#
        );
        fs::write(&path, &original).expect("write structurally invalid state");

        let loaded = load_from_path(&path).expect("recover structurally invalid state");

        assert_eq!(loaded.state, ProjectState::default());
        let notice = loaded
            .notice
            .expect("an unavoidable reset must be reported");
        assert!(notice.contains(".json.corrupt-"), "{notice}");
        assert!(!path.exists());
        let backups = corrupt_backups_of(&path);
        assert_eq!(backups.len(), 1, "expected exactly one backup: {backups:?}");
        assert_eq!(
            fs::read_to_string(&backups[0]).expect("read backup"),
            original,
            "the backup must preserve the user's bytes verbatim"
        );
        for backup in backups {
            let _ = fs::remove_file(backup);
        }
    }

    #[test]
    fn older_state_version_is_upgraded_in_place_and_preserves_workspaces() {
        let path = unique_temp_file();
        let older = STATE_VERSION - 1;
        fs::write(
            &path,
            format!(
                r#"{{"version":{older},"next_workspace_id":2,"next_chat_id":1,"next_terminal_id":2,"workspaces":[{{"id":1,"name":"kept","cwd":null,"environment":{{}},"chats":[],"terminals":[{{"id":1,"name":"shell","status":"Stopped"}}]}}]}}"#
            ),
        )
        .expect("write older state");

        let loaded = load_from_path(&path).expect("load older state");
        let state = loaded.state;

        assert_eq!(loaded.notice, None);
        assert_eq!(state.version, STATE_VERSION);
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].name, "kept");
        assert_eq!(state.workspaces[0].terminals.len(), 1);
        // Upgrading happens in memory only: nothing was renamed aside and the
        // file itself is untouched until the next save.
        assert!(path.exists());
        assert!(corrupt_backups_of(&path).is_empty());
        let _ = fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn backup_rename_failure_surfaces_as_error_and_leaves_state_intact() {
        // Root ignores directory write permissions, so the read-only parent
        // would not stop the rename and the test would assert nothing.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let parent = unique_temp_dir();
        fs::create_dir_all(&parent).expect("create temp state dir");
        let path = parent.join("state.json");
        fs::write(&path, "{not json").expect("write corrupt state");
        let original_mode = fs::metadata(&parent)
            .expect("parent metadata")
            .permissions()
            .mode();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500))
            .expect("make parent read-only");

        let error = load_from_path(&path).expect_err("unrenamable backup should fail the load");

        assert!(
            matches!(error, StateError::BackupFailed { .. }),
            "{error:?}"
        );
        // The user's file is still exactly where it was: a failed backup must
        // never reset the state anyway.
        fs::set_permissions(&parent, fs::Permissions::from_mode(original_mode))
            .expect("restore parent permissions");
        assert_eq!(
            fs::read_to_string(&path).expect("state remains"),
            "{not json"
        );
        fs::remove_dir_all(&parent).expect("remove temp state dir");
    }

    #[cfg(unix)]
    #[test]
    fn state_behind_a_symlink_is_refused_and_nothing_is_backed_up() {
        let parent = unique_temp_dir();
        fs::create_dir_all(&parent).expect("create temp state dir");
        let target = parent.join("planted.json");
        fs::write(&target, "{}").expect("write target");
        let path = parent.join("state.json");
        std::os::unix::fs::symlink(&target, &path).expect("create symlink");

        let error = load_from_path(&path).expect_err("a symlinked state file must be refused");

        // Refusing is not the same as "corrupt": a file we would not read must
        // never be renamed aside, or a refusal would destroy the real state.
        assert!(
            matches!(&error, StateError::Io { source, .. } if source.kind() != io::ErrorKind::NotFound),
            "{error:?}"
        );
        assert!(corrupt_backups_of(&path).is_empty());
        assert!(fs::symlink_metadata(&path).is_ok());
        fs::remove_dir_all(&parent).expect("remove temp state dir");
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_state_is_refused() {
        let parent = unique_temp_dir();
        fs::create_dir_all(&parent).expect("create temp state dir");
        let path = parent.join("state.json");
        fs::write(&path, "{}").expect("write state");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("chmod");

        let error = load_from_path(&path).expect_err("a world-writable state must be refused");

        assert!(
            matches!(&error, StateError::Io { source, .. } if source.kind() == io::ErrorKind::PermissionDenied),
            "{error:?}"
        );
        assert!(corrupt_backups_of(&path).is_empty());
        fs::remove_dir_all(&parent).expect("remove temp state dir");
    }

    fn migration_copies_of(path: &Path) -> Vec<PathBuf> {
        backups_with_prefix(path, "json.pre-11a-")
    }

    fn corrupt_backups_of(path: &Path) -> Vec<PathBuf> {
        backups_with_prefix(path, "json.corrupt-")
    }

    fn backups_with_prefix(path: &Path, suffix: &str) -> Vec<PathBuf> {
        let parent = path.parent().expect("temp path has parent");
        let stem = path
            .file_stem()
            .expect("temp path has stem")
            .to_string_lossy();
        let prefix = format!("{stem}.{suffix}");
        fs::read_dir(parent)
            .expect("read state parent")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|entry| {
                entry
                    .file_name()
                    .map(|name| name.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn assert_no_state_temp_files(path: &Path) {
        let parent = path.parent().expect("temp path has parent");
        let prefix = format!(
            "{}.tmp-{}-",
            path.file_name().unwrap().to_string_lossy(),
            std::process::id()
        );
        let matches = fs::read_dir(parent)
            .expect("read state parent")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .collect::<Vec<_>>();
        assert!(matches.is_empty(), "left state temp files: {matches:?}");
    }

    fn unique_temp_file() -> PathBuf {
        unique_temp_dir().with_extension("json")
    }

    fn unique_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-storage-test-{unique}"))
    }
}
