use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::model::ProjectState;

const STATE_PATH_ENV: &str = "MULT_STATE_PATH";

pub fn load_or_default() -> io::Result<ProjectState> {
    let path = state_path();

    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(invalid_data),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ProjectState::default()),
        Err(error) => Err(error),
    }
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

fn save_to_path(state: &ProjectState, path: &Path) -> io::Result<()> {
    ensure_parent_dir(path)?;

    let json = serde_json::to_string_pretty(state).map_err(invalid_data)?;
    write_atomically(path, format!("{json}\n").as_bytes())
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
    for attempt in 0..16 {
        let temp_path = temp_save_path(path, attempt);
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
        fs::create_dir_all(parent)?;
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

fn temp_save_path(path: &Path, attempt: usize) -> PathBuf {
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
        assert!(!temp_save_path(&path, 0).exists());
    }

    #[test]
    fn save_does_not_clobber_preexisting_temp_file() {
        let path = unique_temp_file();
        let preexisting_temp = temp_save_path(&path, 0);
        fs::write(&preexisting_temp, "do not clobber").expect("write preexisting temp file");

        save_to_path(&ProjectState::default(), &path).expect("save state");

        assert_eq!(
            fs::read_to_string(&preexisting_temp).expect("preexisting temp remains"),
            "do not clobber"
        );
        fs::remove_file(&preexisting_temp).expect("remove preexisting temp file");
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

    #[test]
    fn parent_dir_creation_allows_bare_file_names() {
        ensure_parent_dir(Path::new("state.json"))
            .expect("bare file name has no directory to create");
    }

    fn unique_temp_file() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        env::temp_dir().join(format!("mult-storage-test-{unique}.json"))
    }
}
