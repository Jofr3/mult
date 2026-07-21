use std::{
    cell::RefCell,
    collections::BTreeMap,
    env,
    ffi::{CStr, CString, OsStr, OsString},
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, RawFd},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;

use crate::{
    model::{
        AgentKind, ChatId, ChatMessage, ChatSession, ChatStatus, IdentitySource, ProjectState,
        SecureIdentitySource, SessionIdentities, StateNamespace, TerminalId, TerminalLaunch,
        TerminalSession, TerminalStatus, Workspace, WorkspaceId, STATE_VERSION,
    },
    paths,
};

const STATE_PATH_ENV: &str = "MULT_STATE_PATH";
const LOCK_SUFFIX: &str = ".lock";
const CORRUPT_SUFFIX: &str = ".corrupt-";
const MAX_TEMP_ATTEMPTS: usize = 64;

pub struct StateStore {
    paths: StatePaths,
    directory: SecureDirectory,
    _lock: File,
    active_runtime_save: bool,
}

struct ActiveRuntimeStore {
    directory: SecureDirectory,
    state_name: OsString,
}

thread_local! {
    static ACTIVE_RUNTIME_STORE: RefCell<Option<ActiveRuntimeStore>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    state: PathBuf,
    normalize_parent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedState {
    pub state: ProjectState,
    pub needs_save: bool,
}

impl StatePaths {
    pub fn resolve() -> io::Result<Self> {
        if let Some(path) = env::var_os(STATE_PATH_ENV) {
            return Self::from_explicit_path(PathBuf::from(path));
        }

        Self::new(paths::data_home()?.join("mult").join("state.json"), true)
    }

    pub fn from_explicit_path(path: PathBuf) -> io::Result<Self> {
        Self::new(path, false)
    }

    fn new(state: PathBuf, normalize_parent: bool) -> io::Result<Self> {
        if state.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("state path must name a file: {}", state.display()),
            ));
        }
        Ok(Self {
            state,
            normalize_parent,
        })
    }

    pub fn state_path(&self) -> &Path {
        &self.state
    }

    fn state_name(&self) -> &OsStr {
        self.state
            .file_name()
            .expect("StatePaths validates the state file name")
    }

    fn lock_name(&self) -> OsString {
        let mut name = self.state_name().to_os_string();
        name.push(LOCK_SUFFIX);
        name
    }
}

impl StateStore {
    pub fn acquire_default() -> io::Result<Self> {
        let mut store = Self::acquire(StatePaths::resolve()?)?;
        store.activate_runtime_saves()?;
        Ok(store)
    }

    pub fn acquire(paths: StatePaths) -> io::Result<Self> {
        let directory =
            SecureDirectory::open_parent(paths.state_path(), true, paths.normalize_parent)?;
        let lock_name = paths.lock_name();
        let lock = directory.open_file(
            &lock_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK,
            0o600,
        )?;
        validate_private_regular_file(&lock, &format!("state lock {}", paths.state.display()))?;
        set_file_mode(&lock, 0o600)?;

        let flock_status = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if flock_status != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK)
                || error.raw_os_error() == Some(libc::EAGAIN)
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "another mult process owns state path {}",
                        paths.state.display()
                    ),
                ));
            }
            return Err(error);
        }

        let store = Self {
            paths,
            directory,
            _lock: lock,
            active_runtime_save: false,
        };
        store.normalize_corrupt_backups()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        self.paths.state_path()
    }

    pub fn load_or_default(&self) -> io::Result<LoadedState> {
        let mut source = SecureIdentitySource::new()?;
        self.load_with_identity_source(&mut source)
    }

    pub fn save(&self, state: &ProjectState) -> io::Result<()> {
        validate_current_state(state)?;
        save_to_directory(state, &self.directory, self.paths.state_name())
    }

    fn activate_runtime_saves(&mut self) -> io::Result<()> {
        let active = ActiveRuntimeStore {
            directory: self.directory.try_clone()?,
            state_name: self.paths.state_name().to_os_string(),
        };
        ACTIVE_RUNTIME_STORE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a state store is already active on this thread",
                ));
            }
            *slot = Some(active);
            self.active_runtime_save = true;
            Ok(())
        })
    }

    fn load_with_identity_source(
        &self,
        source: &mut impl IdentitySource,
    ) -> io::Result<LoadedState> {
        let bytes = match self.read_state_bytes()? {
            Some(bytes) => bytes,
            None => {
                return Ok(LoadedState {
                    state: ProjectState::try_default_with(source)?,
                    needs_save: true,
                });
            }
        };

        match decode_state(&bytes) {
            Ok(DecodedState::V1(old)) => Ok(LoadedState {
                state: migrate_v1_to_v2(old, source)?,
                needs_save: true,
            }),
            Ok(DecodedState::V2(mut state)) => {
                validate_current_state(&state)?;
                let ids_normalized = state
                    .normalize_next_ids()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                let statuses_normalized = normalize_unowned_agent_statuses(&mut state);
                validate_current_state(&state)?;
                Ok(LoadedState {
                    state,
                    needs_save: ids_normalized || statuses_normalized,
                })
            }
            Err(StateDecodeError::InvalidJson(error)) => {
                // Construct the replacement first. Entropy failure therefore
                // leaves the invalid source exactly where it was.
                let state = ProjectState::try_default_with(source)?;
                self.backup_invalid_state(error)?;
                Ok(LoadedState {
                    state,
                    needs_save: true,
                })
            }
            Err(StateDecodeError::UnsupportedVersion(version)) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "state file version {version} is unsupported (current version is {STATE_VERSION}); not modifying {}",
                    self.paths.state.display()
                ),
            )),
        }
    }

    fn read_state_bytes(&self) -> io::Result<Option<Vec<u8>>> {
        let mut file = match self.directory.open_file(
            self.paths.state_name(),
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        validate_private_regular_file(
            &file,
            &format!("state file {}", self.paths.state.display()),
        )?;
        set_file_mode(&file, 0o600)?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    fn backup_invalid_state(&self, decode_error: serde_json::Error) -> io::Result<()> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let mut source = SecureIdentitySource::new()?;

        for _ in 0..MAX_TEMP_ATTEMPTS {
            let suffix = random_u64(&mut source)?;
            let mut backup_name = self.paths.state_name().to_os_string();
            backup_name.push(format!("{CORRUPT_SUFFIX}{timestamp}-{suffix:016x}"));
            if self.directory.exists_no_follow(&backup_name)? {
                continue;
            }

            if let Err(rename_error) = self.directory.rename(self.paths.state_name(), &backup_name)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "state JSON is invalid ({decode_error}); failed to move {} to {}: {rename_error}",
                        self.paths.state.display(),
                        self.paths
                            .state
                            .with_file_name(&backup_name)
                            .display()
                    ),
                ));
            }

            let backup =
                self.directory
                    .open_file(&backup_name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
            validate_private_regular_file(&backup, "corrupt state backup")?;
            set_file_mode(&backup, 0o600)?;
            self.directory.sync()?;
            return Ok(());
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not choose a unique corrupt-state backup name",
        ))
    }

    fn normalize_corrupt_backups(&self) -> io::Result<()> {
        let mut prefix = self.paths.state_name().to_os_string();
        prefix.push(CORRUPT_SUFFIX);
        let prefix = prefix.as_bytes();

        for name in self.directory.entry_names()? {
            if !name.as_bytes().starts_with(prefix) {
                continue;
            }
            let file = match self
                .directory
                .open_file(&name, libc::O_RDONLY | libc::O_NONBLOCK, 0)
            {
                Ok(file) => file,
                // Unsafe entries are ignored rather than followed or replaced.
                Err(_) => continue,
            };
            if validate_private_regular_file(&file, "corrupt state backup").is_ok() {
                set_file_mode(&file, 0o600)?;
            }
        }
        Ok(())
    }
}

impl Drop for StateStore {
    fn drop(&mut self) {
        if self.active_runtime_save {
            ACTIVE_RUNTIME_STORE.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
}

/// Compatibility entry point for the current runtime. After startup acquires a
/// default [`StateStore`], saves use a cloned descriptor for that exact parent
/// directory rather than re-resolving an environment path or following a
/// replacement directory.
pub fn save(state: &ProjectState) -> io::Result<()> {
    validate_current_state(state)?;
    if let Some(result) = ACTIVE_RUNTIME_STORE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|active| save_to_directory(state, &active.directory, &active.state_name))
    }) {
        return result;
    }

    // Library callers outside the TUI must acquire the same lifetime lock for
    // their complete write. If another process/thread owns it, fail instead of
    // bypassing that authority with an unlocked atomic rename.
    let store = StateStore::acquire(StatePaths::resolve()?)?;
    store.save(state)
}

pub fn state_path() -> PathBuf {
    StatePaths::resolve()
        .map(|paths| paths.state)
        .unwrap_or_else(|_| PathBuf::from("<state path unavailable>"))
}

fn validate_current_state(state: &ProjectState) -> io::Result<()> {
    if state.version != STATE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "refusing to save state version {}; expected {STATE_VERSION}",
                state.version
            ),
        ));
    }
    state
        .validate_session_identities()
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))
}

fn decode_state(bytes: &[u8]) -> Result<DecodedState, StateDecodeError> {
    // Decode only the envelope before dispatch. Unknown future enum variants
    // must never make a future file look corrupt and trigger backup/reset.
    let envelope: StateVersionEnvelope =
        serde_json::from_slice(bytes).map_err(StateDecodeError::InvalidJson)?;
    match envelope.version {
        1 => serde_json::from_slice(bytes)
            .map(DecodedState::V1)
            .map_err(StateDecodeError::InvalidJson),
        STATE_VERSION => serde_json::from_slice(bytes)
            .map(DecodedState::V2)
            .map_err(StateDecodeError::InvalidJson),
        version => Err(StateDecodeError::UnsupportedVersion(version)),
    }
}

fn migrate_v1_to_v2(old: StateV1, source: &mut impl IdentitySource) -> io::Result<ProjectState> {
    let namespace = next_namespace(source)?;
    let mut state = ProjectState {
        version: STATE_VERSION,
        namespace,
        session_identities: SessionIdentities::default(),
        agent_generations: BTreeMap::new(),
        next_workspace_id: old.next_workspace_id,
        next_chat_id: old.next_chat_id,
        next_terminal_id: old.next_terminal_id,
        workspaces: old
            .workspaces
            .into_iter()
            .map(|workspace| Workspace {
                id: workspace.id,
                name: workspace.name,
                cwd: workspace.cwd,
                environment: workspace.environment,
                chats: workspace
                    .chats
                    .into_iter()
                    .map(|chat| ChatSession {
                        id: chat.id,
                        name: chat.name,
                        status: match chat.status {
                            ChatStatus::Thinking | ChatStatus::Waiting => ChatStatus::Idle,
                            status => status,
                        },
                        agent: chat.agent,
                        messages: chat.messages,
                    })
                    .collect(),
                terminals: workspace
                    .terminals
                    .into_iter()
                    .map(|terminal| TerminalSession {
                        id: terminal.id,
                        name: terminal.name,
                        status: terminal.status,
                        launch: terminal.launch,
                    })
                    .collect(),
            })
            .collect(),
    };

    state
        .normalize_next_ids()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    state.assign_session_identities(source)?;
    validate_current_state(&state)?;
    Ok(state)
}

fn normalize_unowned_agent_statuses(state: &mut ProjectState) -> bool {
    let active = state.agent_generations.keys().copied().collect::<Vec<_>>();
    let mut changed = false;
    for chat in state
        .workspaces
        .iter_mut()
        .flat_map(|workspace| workspace.chats.iter_mut())
    {
        if !active.contains(&chat.id)
            && matches!(chat.status, ChatStatus::Thinking | ChatStatus::Waiting)
        {
            chat.status = ChatStatus::Idle;
            changed = true;
        }
    }
    changed
}

fn next_namespace(source: &mut impl IdentitySource) -> io::Result<StateNamespace> {
    for _ in 0..64 {
        let mut bytes = [0_u8; 16];
        source.fill_bytes(&mut bytes)?;
        if let Some(namespace) = StateNamespace::from_bytes(bytes) {
            return Ok(namespace);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "secure entropy repeatedly produced an all-zero state namespace",
    ))
}

fn save_to_directory(
    state: &ProjectState,
    directory: &SecureDirectory,
    state_name: &OsStr,
) -> io::Result<()> {
    let json = serde_json::to_string_pretty(state).map_err(invalid_data)?;
    let contents = format!("{json}\n");
    let mut source = SecureIdentitySource::new()?;

    for _ in 0..MAX_TEMP_ATTEMPTS {
        let mut temp_name = state_name.to_os_string();
        temp_name.push(format!(
            ".tmp-{}-{:016x}",
            std::process::id(),
            random_u64(&mut source)?
        ));
        let mut file = match directory.open_file(
            &temp_name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let result = (|| {
            validate_private_regular_file(&file, "temporary state file")?;
            set_file_mode(&file, 0o600)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            drop(file);
            directory.rename(&temp_name, state_name)?;
            directory.sync()
        })();
        if result.is_err() {
            let _ = directory.unlink(&temp_name);
        }
        return result;
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temporary state file",
    ))
}

fn random_u64(source: &mut impl IdentitySource) -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    source.fill_bytes(&mut bytes)?;
    Ok(u64::from_ne_bytes(bytes))
}

#[derive(Debug, Deserialize)]
struct StateVersionEnvelope {
    version: u32,
}

enum DecodedState {
    V1(StateV1),
    V2(ProjectState),
}

#[derive(Debug)]
enum StateDecodeError {
    InvalidJson(serde_json::Error),
    UnsupportedVersion(u32),
}

#[derive(Debug, Deserialize)]
struct StateV1 {
    #[allow(dead_code)]
    version: u32,
    next_workspace_id: u64,
    next_chat_id: u64,
    next_terminal_id: u64,
    workspaces: Vec<WorkspaceV1>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceV1 {
    id: WorkspaceId,
    name: String,
    cwd: Option<PathBuf>,
    environment: BTreeMap<String, String>,
    chats: Vec<ChatV1>,
    terminals: Vec<TerminalV1>,
}

#[derive(Debug, Deserialize)]
struct ChatV1 {
    id: ChatId,
    name: String,
    status: ChatStatus,
    #[serde(default)]
    agent: AgentKind,
    #[serde(default)]
    messages: Vec<ChatMessage>,
}

#[derive(Debug, Deserialize)]
struct TerminalV1 {
    id: TerminalId,
    name: String,
    status: TerminalStatus,
    #[serde(default)]
    launch: TerminalLaunch,
}

pub(crate) struct SecureDirectory {
    file: File,
}

impl SecureDirectory {
    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
        })
    }

    pub(crate) fn open_parent(
        path: &Path,
        create: bool,
        normalize_parent: bool,
    ) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut current = if parent.is_absolute() {
            open_directory_at(libc::AT_FDCWD, OsStr::new("/"))?
        } else {
            open_directory_at(libc::AT_FDCWD, OsStr::new("."))?
        };

        for component in parent.components() {
            let name = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::ParentDir => OsStr::new(".."),
                Component::Normal(name) => name,
                Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "unsupported state path prefix",
                    ));
                }
            };

            let next = match open_directory_at(current.as_raw_fd(), name) {
                Ok(directory) => directory,
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    mkdir_at(current.as_raw_fd(), name, 0o700)?;
                    open_directory_at(current.as_raw_fd(), name)?
                }
                Err(error) => return Err(error),
            };
            current = next;
        }

        validate_directory(&current, parent)?;
        validate_authority_directory(&current, parent)?;
        if normalize_parent {
            set_file_mode(&current, 0o700)?;
        }
        Ok(Self { file: current })
    }

    pub(crate) fn open_file(&self, name: &OsStr, flags: i32, mode: u32) -> io::Result<File> {
        let name = c_string(name)?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::mode_t,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn exists_no_follow(&self, name: &OsStr) -> io::Result<bool> {
        match self.open_file(name, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        let from = c_string(from)?;
        let to = c_string(to)?;
        let status = unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                from.as_ptr(),
                self.file.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn unlink(&self, name: &OsStr) -> io::Result<()> {
        let name = c_string(name)?;
        let status = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn entry_names(&self) -> io::Result<Vec<OsString>> {
        let duplicate = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        let directory = unsafe { libc::fdopendir(duplicate) };
        if directory.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(duplicate);
            }
            return Err(error);
        }

        let mut names = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(directory) };
            if entry.is_null() {
                break;
            }
            let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes != b"." && bytes != b".." {
                names.push(OsString::from_vec(bytes.to_vec()));
            }
        }
        if unsafe { libc::closedir(directory) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(names)
    }
}

fn open_directory_at(parent: RawFd, name: &OsStr) -> io::Result<File> {
    let name = c_string(name)?;
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn mkdir_at(parent: RawFd, name: &OsStr, mode: u32) -> io::Result<()> {
    let name = c_string(name)?;
    let status = unsafe { libc::mkdirat(parent, name.as_ptr(), mode as libc::mode_t) };
    if status == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(error)
        }
    }
}

fn validate_directory(file: &File, path: &Path) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("state parent is not a directory: {}", path.display()),
        ));
    }
    Ok(())
}

pub(crate) fn validate_owned_directory(file: &File, path: &Path) -> io::Result<()> {
    let metadata = file.metadata()?;
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "private state directory is not owned by the effective user: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_authority_directory(file: &File, path: &Path) -> io::Result<()> {
    validate_owned_directory(file, path)?;
    let metadata = file.metadata()?;
    if metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "state parent is writable by group or others, so its lock inode is replaceable: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn validate_private_regular_file(file: &File, description: &str) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{description} is not a regular file"),
        ));
    }
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{description} is not owned by the effective user"),
        ));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{description} has multiple hard links"),
        ));
    }
    Ok(())
}

pub(crate) fn set_file_mode(file: &File, mode: u32) -> io::Result<()> {
    let status = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "state path contains an interior NUL byte",
        )
    })
}

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(1);

    struct FixedIdentitySource {
        bytes: Vec<u8>,
        offset: usize,
    }

    impl FixedIdentitySource {
        fn sequential(length: usize) -> Self {
            Self {
                bytes: (0..length).map(|index| index as u8).collect(),
                offset: 0,
            }
        }
    }

    impl IdentitySource for FixedIdentitySource {
        fn fill_bytes(&mut self, output: &mut [u8]) -> io::Result<()> {
            let end = self.offset.saturating_add(output.len());
            let input = self.bytes.get(self.offset..end).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "fixed entropy exhausted")
            })?;
            output.copy_from_slice(input);
            self.offset = end;
            Ok(())
        }
    }

    struct FailingIdentitySource;

    impl IdentitySource for FailingIdentitySource {
        fn fill_bytes(&mut self, _output: &mut [u8]) -> io::Result<()> {
            Err(io::Error::other("injected entropy failure"))
        }
    }

    #[test]
    fn process_lifetime_lock_rejects_a_second_owner_and_releases_on_drop() {
        let path = unique_test_dir().join("state.json");
        let paths = StatePaths::from_explicit_path(path.clone()).unwrap();
        let first = StateStore::acquire(paths.clone()).expect("acquire first state owner");

        let error = StateStore::acquire(paths.clone())
            .err()
            .expect("second state owner must fail");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error
            .to_string()
            .contains("another mult process owns state path"));
        assert_eq!(
            fs::metadata(path.with_file_name("state.json.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        drop(first);
        StateStore::acquire(paths).expect("lock is released when the owner is dropped");
    }

    #[test]
    fn explicit_state_path_rejects_a_replaceable_parent_directory() {
        let parent = unique_test_dir();
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        let path = parent.join("state.json");

        let error = StateStore::acquire(StatePaths::from_explicit_path(path).unwrap())
            .err()
            .expect("world-writable parent must not anchor the lifetime lock");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn runtime_saves_keep_using_the_locked_parent_directory_descriptor() {
        let root = unique_test_dir();
        let parent = root.join("owned");
        let moved_parent = root.join("moved");
        let path = parent.join("state.json");
        let mut store =
            StateStore::acquire(StatePaths::from_explicit_path(path.clone()).unwrap()).unwrap();
        store.activate_runtime_saves().unwrap();
        let mut state = ProjectState::try_default().unwrap();
        store.save(&state).unwrap();

        fs::rename(&parent, &moved_parent).unwrap();
        fs::create_dir(&parent).unwrap();
        state.workspaces[0].name = "saved through descriptor".to_string();
        save(&state).unwrap();

        assert!(!path.exists());
        let saved: ProjectState =
            serde_json::from_slice(&fs::read(moved_parent.join("state.json")).unwrap()).unwrap();
        assert_eq!(saved.workspaces[0].name, "saved through descriptor");
        drop(store);
    }

    #[test]
    fn unwinding_releases_the_process_lifetime_lock() {
        let path = unique_test_dir().join("state.json");
        let paths = StatePaths::from_explicit_path(path).unwrap();

        let unwind = std::panic::catch_unwind({
            let paths = paths.clone();
            move || {
                let _owner = StateStore::acquire(paths).unwrap();
                panic!("injected panic while owning state");
            }
        });

        assert!(unwind.is_err());
        StateStore::acquire(paths).expect("unwind must close and release the lock descriptor");
    }

    #[test]
    fn missing_state_returns_new_identity_with_explicit_needs_save() {
        let path = unique_test_dir().join("state.json");
        let store = StateStore::acquire(StatePaths::from_explicit_path(path).unwrap()).unwrap();
        let loaded = store.load_or_default().unwrap();

        assert!(loaded.needs_save);
        assert_eq!(loaded.state.version, STATE_VERSION);
        loaded.state.validate_session_identities().unwrap();
    }

    #[test]
    fn v1_original_shape_migrates_to_exact_golden_v2() {
        assert_golden_migration(
            include_bytes!("../tests/fixtures/state/v1-original.json"),
            include_bytes!("../tests/fixtures/state/v1-original.expected-v2.json"),
        );
    }

    #[test]
    fn v1_current_shape_migrates_to_exact_golden_v2() {
        assert_golden_migration(
            include_bytes!("../tests/fixtures/state/v1-current.json"),
            include_bytes!("../tests/fixtures/state/v1-current.expected-v2.json"),
        );
    }

    #[test]
    fn canonical_v2_fixture_does_not_need_save() {
        let path = unique_test_dir().join("state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            include_bytes!("../tests/fixtures/state/v2-current.json"),
        )
        .unwrap();
        let store = StateStore::acquire(StatePaths::from_explicit_path(path).unwrap()).unwrap();
        let loaded = store.load_or_default().unwrap();

        assert!(!loaded.needs_save);
        assert_eq!(loaded.state.version, STATE_VERSION);
        loaded.state.validate_session_identities().unwrap();
    }

    #[test]
    fn migration_entropy_failure_preserves_source_bytes() {
        let path = unique_test_dir().join("state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = include_bytes!("../tests/fixtures/state/v1-current.json");
        fs::write(&path, original).unwrap();
        let store =
            StateStore::acquire(StatePaths::from_explicit_path(path.clone()).unwrap()).unwrap();

        let error = store
            .load_with_identity_source(&mut FailingIdentitySource)
            .unwrap_err();

        assert!(error.to_string().contains("injected entropy failure"));
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn future_state_is_rejected_and_preserved_byte_for_byte() {
        let path = unique_test_dir().join("state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = include_bytes!("../tests/fixtures/state/future-v3-unknown.json");
        fs::write(&path, original).unwrap();
        let store =
            StateStore::acquire(StatePaths::from_explicit_path(path.clone()).unwrap()).unwrap();

        let error = store.load_or_default().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn migration_keeps_command_as_data_and_does_not_execute_it() {
        let root = unique_test_dir();
        let marker = root.join("command-was-executed");
        let command = format!("touch {}", marker.display());
        let input = format!(
            r#"{{"version":1,"next_workspace_id":2,"next_chat_id":1,"next_terminal_id":2,"workspaces":[{{"id":1,"name":"w","cwd":null,"environment":{{}},"chats":[],"terminals":[{{"id":1,"name":"cmd","status":"Running","launch":{{"kind":"command","command":{}}}}}]}}]}}"#,
            serde_json::to_string(&command).unwrap()
        );
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.json");
        fs::write(&path, input).unwrap();
        let store = StateStore::acquire(StatePaths::from_explicit_path(path).unwrap()).unwrap();
        let mut source = FixedIdentitySource::sequential(64);

        let loaded = store.load_with_identity_source(&mut source).unwrap();

        assert!(loaded.needs_save);
        assert!(!marker.exists());
        assert_eq!(
            loaded.state.workspaces[0].terminals[0].launch,
            TerminalLaunch::Command(command)
        );
    }

    #[test]
    fn state_and_safe_backup_modes_are_normalized_without_following_symlinks() {
        let root = unique_test_dir();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.json");
        fs::write(
            &path,
            include_bytes!("../tests/fixtures/state/v2-current.json"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        let backup = root.join("state.json.corrupt-old");
        fs::write(&backup, b"backup").unwrap();
        fs::set_permissions(&backup, fs::Permissions::from_mode(0o666)).unwrap();
        let symlink_target = root.join("backup-target");
        fs::write(&symlink_target, b"do not chmod through link").unwrap();
        fs::set_permissions(&symlink_target, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&symlink_target, root.join("state.json.corrupt-hostile")).unwrap();

        let store =
            StateStore::acquire(StatePaths::from_explicit_path(path.clone()).unwrap()).unwrap();
        store.load_or_default().unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(symlink_target).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn hostile_state_and_lock_symlinks_are_rejected() {
        let root = unique_test_dir();
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target");
        fs::write(&target, b"do not touch").unwrap();

        let state_path = root.join("state.json");
        symlink(&target, &state_path).unwrap();
        let store =
            StateStore::acquire(StatePaths::from_explicit_path(state_path.clone()).unwrap())
                .unwrap();
        assert!(store.load_or_default().is_err());
        assert_eq!(fs::read(&target).unwrap(), b"do not touch");
        drop(store);

        fs::remove_file(&state_path).unwrap();
        let lock_path = root.join("state.json.lock");
        fs::remove_file(&lock_path).unwrap();
        symlink(&target, &lock_path).unwrap();
        assert!(StateStore::acquire(StatePaths::from_explicit_path(state_path).unwrap()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"do not touch");
    }

    #[test]
    fn intermediate_directory_symlinks_are_never_followed() {
        let root = unique_test_dir();
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, root.join("linked")).unwrap();
        let path = root.join("linked").join("state.json");

        assert!(StateStore::acquire(StatePaths::from_explicit_path(path).unwrap()).is_err());
        assert!(!target.join("state.json.lock").exists());
    }

    #[test]
    fn existing_app_owned_directory_mode_is_normalized() {
        let root = unique_test_dir();
        let parent = root.join("mult");
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        let paths = StatePaths::new(parent.join("state.json"), true).unwrap();

        StateStore::acquire(paths).unwrap();

        assert_eq!(
            fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn save_uses_owner_only_file_and_new_directory_modes() {
        let root = unique_test_dir();
        let path = root.join("nested").join("state.json");
        let store =
            StateStore::acquire(StatePaths::from_explicit_path(path.clone()).unwrap()).unwrap();
        let state = ProjectState::try_default().unwrap();
        store.save(&state).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    fn assert_golden_migration(input: &[u8], expected: &[u8]) {
        let decoded = match decode_state(input).unwrap() {
            DecodedState::V1(state) => state,
            DecodedState::V2(_) => panic!("fixture must be v1"),
        };
        let mut source = FixedIdentitySource::sequential(256);
        let migrated = migrate_v1_to_v2(decoded, &mut source).unwrap();
        let actual = format!("{}\n", serde_json::to_string_pretty(&migrated).unwrap());

        assert_eq!(actual.as_bytes(), expected);
    }

    fn unique_test_dir() -> PathBuf {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "mult-storage-test-{}-{sequence}",
            std::process::id()
        ))
    }
}
