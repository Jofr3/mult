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
    #[serde(default = "default_mouse_capture")]
    pub mouse_capture: bool,
    #[serde(default)]
    pub colorscheme: ColorSchemeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorSchemeConfig {
    #[serde(default = "default_moon_nc", rename = "_nc", alias = "nc")]
    pub nc: String,
    #[serde(default = "default_moon_base")]
    pub base: String,
    #[serde(default = "default_moon_surface")]
    pub surface: String,
    #[serde(default = "default_moon_overlay")]
    pub overlay: String,
    #[serde(default = "default_moon_muted")]
    pub muted: String,
    #[serde(default = "default_moon_subtle")]
    pub subtle: String,
    #[serde(default = "default_moon_text")]
    pub text: String,
    #[serde(default = "default_moon_love")]
    pub love: String,
    #[serde(default = "default_moon_gold")]
    pub gold: String,
    #[serde(default = "default_moon_rose")]
    pub rose: String,
    #[serde(default = "default_moon_pine")]
    pub pine: String,
    #[serde(default = "default_moon_foam")]
    pub foam: String,
    #[serde(default = "default_moon_iris")]
    pub iris: String,
    #[serde(default = "default_moon_leaf")]
    pub leaf: String,
    #[serde(default = "default_moon_highlight_low")]
    pub highlight_low: String,
    #[serde(default = "default_moon_highlight_med")]
    pub highlight_med: String,
    #[serde(default = "default_moon_highlight_high")]
    pub highlight_high: String,
    #[serde(default = "default_moon_none")]
    pub none: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pi_agent_command: default_pi_agent_command(),
            auto_start_pi_agent: default_auto_start_pi_agent(),
            auto_start_terminals: default_auto_start_terminals(),
            mouse_capture: default_mouse_capture(),
            colorscheme: ColorSchemeConfig::default(),
        }
    }
}

impl Default for ColorSchemeConfig {
    fn default() -> Self {
        Self {
            nc: default_moon_nc(),
            base: default_moon_base(),
            surface: default_moon_surface(),
            overlay: default_moon_overlay(),
            muted: default_moon_muted(),
            subtle: default_moon_subtle(),
            text: default_moon_text(),
            love: default_moon_love(),
            gold: default_moon_gold(),
            rose: default_moon_rose(),
            pine: default_moon_pine(),
            foam: default_moon_foam(),
            iris: default_moon_iris(),
            leaf: default_moon_leaf(),
            highlight_low: default_moon_highlight_low(),
            highlight_med: default_moon_highlight_med(),
            highlight_high: default_moon_highlight_high(),
            none: default_moon_none(),
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

fn default_mouse_capture() -> bool {
    true
}

fn default_moon_nc() -> String {
    "#1f1d30".to_string()
}

fn default_moon_base() -> String {
    "#232136".to_string()
}

fn default_moon_surface() -> String {
    "#2a273f".to_string()
}

fn default_moon_overlay() -> String {
    "#393552".to_string()
}

fn default_moon_muted() -> String {
    "#6e6a86".to_string()
}

fn default_moon_subtle() -> String {
    "#908caa".to_string()
}

fn default_moon_text() -> String {
    "#e0def4".to_string()
}

fn default_moon_love() -> String {
    "#eb6f92".to_string()
}

fn default_moon_gold() -> String {
    "#f6c177".to_string()
}

fn default_moon_rose() -> String {
    "#ea9a97".to_string()
}

fn default_moon_pine() -> String {
    "#3e8fb0".to_string()
}

fn default_moon_foam() -> String {
    "#9ccfd8".to_string()
}

fn default_moon_iris() -> String {
    "#c4a7e7".to_string()
}

fn default_moon_leaf() -> String {
    "#95b1ac".to_string()
}

fn default_moon_highlight_low() -> String {
    "#2a283e".to_string()
}

fn default_moon_highlight_med() -> String {
    "#44415a".to_string()
}

fn default_moon_highlight_high() -> String {
    "#56526e".to_string()
}

fn default_moon_none() -> String {
    "NONE".to_string()
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
        assert!(config.mouse_capture);
        assert_eq!(config.colorscheme.base, "#232136");
        assert_eq!(config.colorscheme.nc, "#1f1d30");
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
        assert!(config.mouse_capture);
        assert_eq!(config.colorscheme.text, "#e0def4");
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
    fn config_loads_mouse_capture_flag_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"mouse_capture":false}"#).expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert!(!config.mouse_capture);
    }

    #[test]
    fn config_loads_partial_colorscheme_from_json() {
        let path = unique_temp_file();
        fs::write(
            &path,
            r##"{"colorscheme":{"_nc":"#000001","text":"#ffffff"}}"##,
        )
        .expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert_eq!(config.colorscheme.nc, "#000001");
        assert_eq!(config.colorscheme.text, "#ffffff");
        assert_eq!(config.colorscheme.base, "#232136");
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
