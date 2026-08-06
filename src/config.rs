use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde::{Deserialize, Deserializer, Serialize};

use crate::{paths, ui::Palette};

const CONFIG_PATH_ENV: &str = "MULT_CONFIG_PATH";
const CONFIG_FILE_NAME: &str = "config.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_pi_agent_command")]
    pub pi_agent_command: String,
    #[serde(default = "default_claude_code_command")]
    pub claude_code_command: String,
    #[serde(default = "default_auto_start_pi_agent")]
    pub auto_start_pi_agent: bool,
    #[serde(default = "default_auto_start_claude_code_agent")]
    pub auto_start_claude_code_agent: bool,
    #[serde(default = "default_auto_start_terminals")]
    pub auto_start_terminals: bool,
    #[serde(default = "default_mouse_capture")]
    pub mouse_capture: bool,
    #[serde(default)]
    pub projects: Vec<ConfiguredProject>,
    #[serde(default)]
    pub colorscheme: ColorSchemeConfig,
}

impl Config {
    /// The parsed color scheme, memoized on the [`ColorSchemeConfig`] it comes
    /// from. See [`ColorSchemeConfig::palette`].
    pub fn palette(&self) -> Palette {
        self.colorscheme.palette()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfiguredProject {
    pub name: String,
    pub path: PathBuf,
}

/// The built-in color scheme (Rosé Pine Moon), as the hex strings a user would
/// write in `config.json`.
///
/// This is the *only* place those values are spelled out: [`ColorSchemeConfig`]
/// defaults to them, and `ui` derives its compile-time fallback `Color`s from
/// the same literals, so the config text and the rendered defaults cannot drift
/// apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultColorScheme {
    pub nc: &'static str,
    pub base: &'static str,
    pub muted: &'static str,
    pub text: &'static str,
    pub love: &'static str,
    pub gold: &'static str,
    pub pine: &'static str,
    pub foam: &'static str,
    pub iris: &'static str,
    pub highlight_med: &'static str,
    pub cursor: &'static str,
    pub success: &'static str,
}

pub const DEFAULT_COLOR_SCHEME: DefaultColorScheme = DefaultColorScheme {
    nc: "#1f1d30",
    base: "#232136",
    muted: "#6e6a86",
    text: "#e0def4",
    love: "#eb6f92",
    gold: "#f6c177",
    pine: "#3e8fb0",
    foam: "#9ccfd8",
    iris: "#c4a7e7",
    highlight_med: "#44415a",
    cursor: "#ffffff",
    success: "#3e8f54",
};

#[derive(Debug, Serialize, Deserialize)]
pub struct ColorSchemeConfig {
    #[serde(default = "default_moon_nc", rename = "_nc", alias = "nc")]
    pub nc: String,
    #[serde(default = "default_moon_base")]
    pub base: String,
    #[serde(default = "default_moon_muted")]
    pub muted: String,
    #[serde(default = "default_moon_text")]
    pub text: String,
    #[serde(default = "default_moon_love")]
    pub love: String,
    #[serde(default = "default_moon_gold")]
    pub gold: String,
    #[serde(default = "default_moon_pine")]
    pub pine: String,
    #[serde(default = "default_moon_foam")]
    pub foam: String,
    #[serde(default = "default_moon_iris")]
    pub iris: String,
    #[serde(default = "default_moon_highlight_med")]
    pub highlight_med: String,
    #[serde(default = "default_cursor")]
    pub cursor: String,
    #[serde(default = "default_success")]
    pub success: String,
    /// Memoized parse of the twelve keys above, sited here rather than on
    /// `Config` so the cache travels with the exact strings it was derived
    /// from: every way of producing a color scheme — `default`, deserialize,
    /// clone — yields a value with its own empty cell, so no scheme can ever
    /// observe another's palette. The renderer asks once per frame; parsing
    /// twelve hex strings 60 times a second was pure waste.
    ///
    /// Treat the fields above as read-only once a scheme is in use: assigning
    /// one in place after the palette has been observed keeps the old colors.
    /// Nothing does — the render path only ever holds `&Config`.
    #[serde(skip)]
    palette: OnceLock<Palette>,
}

impl ColorSchemeConfig {
    /// The parsed palette. Keys that fail to parse fall back to the built-in
    /// default and are reported by [`Palette::from_colorscheme_reporting`].
    pub fn palette(&self) -> Palette {
        *self.palette.get_or_init(|| Palette::from_colorscheme(self))
    }
}

// The palette is a cache, not state: a clone starts with an empty cell, and
// equality ignores it, so two schemes with the same colors stay equal whether
// or not either has rendered a frame.
impl Clone for ColorSchemeConfig {
    fn clone(&self) -> Self {
        Self {
            nc: self.nc.clone(),
            base: self.base.clone(),
            muted: self.muted.clone(),
            text: self.text.clone(),
            love: self.love.clone(),
            gold: self.gold.clone(),
            pine: self.pine.clone(),
            foam: self.foam.clone(),
            iris: self.iris.clone(),
            highlight_med: self.highlight_med.clone(),
            cursor: self.cursor.clone(),
            success: self.success.clone(),
            palette: OnceLock::new(),
        }
    }
}

impl PartialEq for ColorSchemeConfig {
    fn eq(&self, other: &Self) -> bool {
        self.nc == other.nc
            && self.base == other.base
            && self.muted == other.muted
            && self.text == other.text
            && self.love == other.love
            && self.gold == other.gold
            && self.pine == other.pine
            && self.foam == other.foam
            && self.iris == other.iris
            && self.highlight_med == other.highlight_med
            && self.cursor == other.cursor
            && self.success == other.success
    }
}

impl Eq for ColorSchemeConfig {}

impl Default for Config {
    fn default() -> Self {
        Self {
            pi_agent_command: default_pi_agent_command(),
            claude_code_command: default_claude_code_command(),
            auto_start_pi_agent: default_auto_start_pi_agent(),
            auto_start_claude_code_agent: default_auto_start_claude_code_agent(),
            auto_start_terminals: default_auto_start_terminals(),
            mouse_capture: default_mouse_capture(),
            projects: Vec::new(),
            colorscheme: ColorSchemeConfig::default(),
        }
    }
}

impl<'de> Deserialize<'de> for ConfiguredProject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawProject {
            Object { name: String, path: PathBuf },
            Pair(String, PathBuf),
        }

        match RawProject::deserialize(deserializer)? {
            RawProject::Object { name, path } | RawProject::Pair(name, path) => {
                Ok(Self { name, path })
            }
        }
    }
}

impl Default for ColorSchemeConfig {
    fn default() -> Self {
        Self {
            nc: default_moon_nc(),
            base: default_moon_base(),
            muted: default_moon_muted(),
            text: default_moon_text(),
            love: default_moon_love(),
            gold: default_moon_gold(),
            pine: default_moon_pine(),
            foam: default_moon_foam(),
            iris: default_moon_iris(),
            highlight_med: default_moon_highlight_med(),
            cursor: default_cursor(),
            success: default_success(),
            palette: OnceLock::new(),
        }
    }
}

pub fn load_or_default() -> io::Result<Config> {
    load_from_path(&resolve_config_path()?)
}

pub fn resolve_config_path() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os(CONFIG_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }

    Ok(paths::config_home()?.join("mult").join(CONFIG_FILE_NAME))
}

/// Display-oriented compatibility helper. Loading uses [`resolve_config_path`]
/// and returns an error instead of ever selecting the current directory.
pub fn config_path() -> PathBuf {
    resolve_config_path().unwrap_or_else(|_| PathBuf::from("<configuration path unavailable>"))
}

fn load_from_path(path: &Path) -> io::Result<Config> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(invalid_data),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error),
    }
}

fn default_pi_agent_command() -> String {
    "pi".to_string()
}

fn default_claude_code_command() -> String {
    "claude".to_string()
}

fn default_auto_start_pi_agent() -> bool {
    true
}

fn default_auto_start_claude_code_agent() -> bool {
    true
}

fn default_auto_start_terminals() -> bool {
    true
}

fn default_mouse_capture() -> bool {
    true
}

fn default_moon_nc() -> String {
    DEFAULT_COLOR_SCHEME.nc.to_string()
}

fn default_moon_base() -> String {
    DEFAULT_COLOR_SCHEME.base.to_string()
}

fn default_moon_muted() -> String {
    DEFAULT_COLOR_SCHEME.muted.to_string()
}

fn default_moon_text() -> String {
    DEFAULT_COLOR_SCHEME.text.to_string()
}

fn default_moon_love() -> String {
    DEFAULT_COLOR_SCHEME.love.to_string()
}

fn default_moon_gold() -> String {
    DEFAULT_COLOR_SCHEME.gold.to_string()
}

fn default_moon_pine() -> String {
    DEFAULT_COLOR_SCHEME.pine.to_string()
}

fn default_moon_foam() -> String {
    DEFAULT_COLOR_SCHEME.foam.to_string()
}

fn default_moon_iris() -> String {
    DEFAULT_COLOR_SCHEME.iris.to_string()
}

fn default_moon_highlight_med() -> String {
    DEFAULT_COLOR_SCHEME.highlight_med.to_string()
}

fn default_cursor() -> String {
    DEFAULT_COLOR_SCHEME.cursor.to_string()
}

fn default_success() -> String {
    DEFAULT_COLOR_SCHEME.success.to_string()
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
        assert_eq!(config.claude_code_command, "claude");
        assert!(config.auto_start_pi_agent);
        assert!(config.auto_start_claude_code_agent);
        assert!(config.auto_start_terminals);
        assert!(config.mouse_capture);
        assert!(config.projects.is_empty());
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
        assert!(config.projects.is_empty());
        assert_eq!(config.colorscheme.text, "#e0def4");
    }

    #[test]
    fn config_loads_projects_from_json_objects_and_pairs() {
        let path = unique_temp_file();
        fs::write(
            &path,
            r#"{"projects":[{"name":"mult","path":"~/projects/mult"},["docs","/tmp/docs"]]}"#,
        )
        .expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert_eq!(
            config.projects,
            vec![
                ConfiguredProject {
                    name: "mult".to_string(),
                    path: PathBuf::from("~/projects/mult"),
                },
                ConfiguredProject {
                    name: "docs".to_string(),
                    path: PathBuf::from("/tmp/docs"),
                },
            ]
        );
    }

    #[test]
    fn config_loads_auto_start_flags_from_json() {
        let path = unique_temp_file();
        fs::write(
            &path,
            r#"{"auto_start_pi_agent":false,"auto_start_claude_code_agent":false,"auto_start_terminals":false}"#,
        )
        .expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert_eq!(config.pi_agent_command, "pi");
        assert!(!config.auto_start_pi_agent);
        assert!(!config.auto_start_claude_code_agent);
        assert!(!config.auto_start_terminals);
    }

    #[test]
    fn config_loads_claude_code_command_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"claude_code_command":"claude --resume"}"#).expect("write config");

        let config = load_from_path(&path).expect("load config");

        // The pi command keeps its default while the cc command is overridden.
        assert_eq!(config.pi_agent_command, "pi");
        assert_eq!(config.claude_code_command, "claude --resume");
        assert!(config.auto_start_claude_code_agent);
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
    fn each_color_scheme_memoizes_its_own_palette() {
        let default_config = Config::default();
        let custom = Config {
            colorscheme: ColorSchemeConfig {
                text: "#010203".to_string(),
                ..ColorSchemeConfig::default()
            },
            ..Config::default()
        };

        // Prime the default's cache first: a second scheme built afterwards
        // must still see its own colors, not the one already parsed.
        let default_palette = default_config.palette();
        assert_ne!(custom.palette(), default_palette);
        assert_eq!(custom.palette(), custom.palette());
        assert_eq!(default_config.palette(), default_palette);

        // A clone re-parses its own colors rather than inheriting the cell...
        assert_eq!(custom.clone().palette(), custom.palette());
        // ...and a primed cache does not make two equal configs unequal.
        assert_eq!(default_config, Config::default());
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
