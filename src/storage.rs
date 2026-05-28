use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::model::{ProjectState, STATE_VERSION};

const STATE_PATH_ENV: &str = "MULT_STATE_PATH";

pub fn load_or_default() -> io::Result<ProjectState> {
    load_from_path(&state_path())
}

pub fn save(state: &ProjectState) -> io::Result<()> {
    save_to_path(state, &state_path())
}

pub fn state_path() -> PathBuf {
    if let Some(path) = env::var_os(STATE_PATH_ENV) {
        return PathBuf::from(path);
    }

    data_home().join("mult").join("state.json")
}

fn load_from_path(path: &Path) -> io::Result<ProjectState> {
    match fs::read(path) {
        Ok(bytes) => match decode_project_state(&bytes) {
            Ok(state) => Ok(state),
            Err(StateDecodeError::InvalidJson(error)) => backup_invalid_state_and_reset(path, error),
            Err(StateDecodeError::UnsupportedVersion(version)) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "state file version {version} is newer than supported version {STATE_VERSION}; not modifying {}",
                    path.display()
                ),
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ProjectState::default()),
        Err(error) => Err(error),
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

fn save_to_path(state: &ProjectState, path: &Path) -> io::Result<()> {
    ensure_parent_dir(path)?;

    let json = serde_json::to_string_pretty(state).map_err(invalid_data)?;
    write_atomically(path, format!("{json}\n").as_bytes())
}

fn backup_invalid_state_and_reset(
    path: &Path,
    decode_error: serde_json::Error,
) -> io::Result<ProjectState> {
    let backup = corrupt_backup_path(path);
    fs::rename(path, &backup).map_err(|rename_error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "state JSON is invalid ({decode_error}); failed to move {} to {}: {rename_error}",
                path.display(),
                backup.display()
            ),
        )
    })?;
    Ok(ProjectState::default())
}

fn corrupt_backup_path(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    for attempt in 0..16 {
        let candidate = path.with_extension(format!("json.corrupt-{timestamp}-{attempt}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path.with_extension(format!("json.corrupt-{timestamp}"))
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

fn random_u64() -> io::Result<u64> {
    let mut bytes = [0_u8; 8];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u64::from_ne_bytes(bytes))
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

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use crate::model::TerminalStatus;

    use super::*;

    #[test]
    fn state_path_uses_default_data_home_or_override() {
        let path = state_path();

        if let Some(override_path) = env::var_os(STATE_PATH_ENV) {
            assert_eq!(path, PathBuf::from(override_path));
        } else {
            assert!(path.ends_with("mult/state.json"));
        }
    }

    #[test]
    fn save_preserves_running_terminal_status_for_restart() {
        let path = unique_temp_file();
        let mut state = ProjectState::default();
        state.workspaces[0].terminals[0].status = TerminalStatus::Running;

        save_to_path(&state, &path).expect("save state");

        let bytes = fs::read(&path).expect("read saved state");
        let decoded: ProjectState = serde_json::from_slice(&bytes).expect("decode state");
        assert_eq!(
            decoded.workspaces[0].terminals[0].status,
            TerminalStatus::Running
        );
    }

    #[test]
    fn save_replaces_existing_state_without_leaving_temp_file() {
        let path = unique_temp_file();
        fs::write(&path, "old contents").expect("write old state");
        let mut state = ProjectState::default();
        state.workspaces[0].name = "saved".to_string();

        save_to_path(&state, &path).expect("save state");

        let bytes = fs::read(&path).expect("read saved state");
        let decoded: ProjectState = serde_json::from_slice(&bytes).expect("decode state");
        assert_eq!(decoded.workspaces[0].name, "saved");
        assert_no_state_temp_files(&path);
    }

    #[test]
    fn invalid_state_json_is_backed_up_and_reset() {
        let path = unique_temp_file();
        fs::write(&path, "{not json").expect("write corrupt state");

        let state = load_from_path(&path).expect("recover corrupt state");

        assert_eq!(state, ProjectState::default());
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
        for backup in backups {
            let _ = fs::remove_file(backup.path());
        }
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

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
