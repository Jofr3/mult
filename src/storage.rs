use std::{
    env, fs, io,
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
    let path = state_path();
    let mut snapshot = state.clone();
    snapshot.reset_terminal_statuses();
    save_to_path(&snapshot, &path)
}

pub fn state_path() -> PathBuf {
    if let Some(path) = env::var_os(STATE_PATH_ENV) {
        return PathBuf::from(path);
    }

    data_home().join("mult").join("state.json")
}

fn save_to_path(state: &ProjectState, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(state).map_err(invalid_data)?;
    fs::write(path, format!("{json}\n"))
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
}
