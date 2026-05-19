use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

const CONFIG_PATH_ENV: &str = "MULT_CONFIG_PATH";
const CONFIG_FILE_NAME: &str = "config.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_pi_agent_command")]
    pub pi_agent_command: String,
    #[serde(default = "default_auto_start_pi_agent")]
    pub auto_start_pi_agent: bool,
    #[serde(default = "default_auto_start_terminals")]
    pub auto_start_terminals: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pi_agent_command: default_pi_agent_command(),
            auto_start_pi_agent: default_auto_start_pi_agent(),
            auto_start_terminals: default_auto_start_terminals(),
        }
    }
}

pub fn load_or_default() -> io::Result<Config> {
    load_from_path(&config_path())
}

pub fn config_path() -> PathBuf {
    if let Some(path) = env::var_os(CONFIG_PATH_ENV) {
        return PathBuf::from(path);
    }

    config_home().join("mult").join(CONFIG_FILE_NAME)
}

fn load_from_path(path: &Path) -> io::Result<Config> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(invalid_data),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error),
    }
}

fn config_home() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_pi_agent_command() -> String {
    "pi".to_string()
}

fn default_auto_start_pi_agent() -> bool {
    true
}

fn default_auto_start_terminals() -> bool {
    true
}

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn config_defaults_to_pi_command() {
        let config = Config::default();

        assert_eq!(config.pi_agent_command, "pi");
        assert!(config.auto_start_pi_agent);
        assert!(config.auto_start_terminals);
    }

    #[test]
    fn config_path_uses_config_home_or_override() {
        let path = config_path();

        if let Some(override_path) = env::var_os(CONFIG_PATH_ENV) {
            assert_eq!(path, PathBuf::from(override_path));
        } else {
            assert!(path.ends_with("mult/config.json"));
        }
    }

    #[test]
    fn config_loads_pi_agent_command_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"pi_agent_command":"pi -c"}"#).expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert_eq!(config.pi_agent_command, "pi -c");
        assert!(config.auto_start_pi_agent);
        assert!(config.auto_start_terminals);
    }

    #[test]
    fn config_loads_auto_start_flags_from_json() {
        let path = unique_temp_file();
        fs::write(
            &path,
            r#"{"auto_start_pi_agent":false,"auto_start_terminals":false}"#,
        )
        .expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert_eq!(config.pi_agent_command, "pi");
        assert!(!config.auto_start_pi_agent);
        assert!(!config.auto_start_terminals);
    }

    #[test]
    fn missing_config_uses_defaults() {
        let config = load_from_path(&unique_temp_file()).expect("load missing config");

        assert_eq!(config, Config::default());
    }

    fn unique_temp_file() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("mult-config-test-{unique}.json"))
    }
}
