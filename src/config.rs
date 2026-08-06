use std::{
    env,
    ffi::OsStr,
    fmt, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Deserializer, Serialize};

const CONFIG_PATH_ENV: &str = "MULT_CONFIG_PATH";
const CONFIG_FILE_NAME: &str = "config.json";
/// Honoured per <https://no-color.org>: any non-empty value disables colour.
const NO_COLOR_ENV: &str = "NO_COLOR";

/// Unknown keys are rejected (E6). `auto_start_terminal` — one `s` short of
/// `auto_start_terminals` — used to be accepted and silently ignored, so a user
/// who thought they had turned auto-start off had not. The same rule makes a
/// stale key from an older release a loud error instead of a setting that
/// quietly stopped applying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Whether a text selection is pushed to the system clipboard with OSC 52.
    ///
    /// Defaults to `true`, which is the behaviour that has always been there.
    /// It is a toggle rather than an always-on feature because the payload is
    /// untrusted PTY output — up to a screenful of whatever a program printed —
    /// and OSC 52 puts it in the clipboard of the *host* terminal, outside
    /// `mult`'s control. Turning it off keeps selection and `Ctrl+Shift+C`
    /// working as a no-op instead of removing the binding.
    #[serde(default = "default_clipboard_osc52")]
    pub clipboard_osc52: bool,
    #[serde(default)]
    pub projects: Vec<ConfiguredProject>,
    #[serde(default)]
    pub colorscheme: ColorSchemeConfig,
    /// Memo backing [`Config::colors`]; derived from [`Self::colorscheme`], not
    /// configuration in its own right, so serde skips it.
    #[serde(skip)]
    pub color_cache: ColorCache,
    /// Whether the renderer may emit colour at all.
    ///
    /// Not a config key: it comes from `$NO_COLOR` (E10) and is therefore set by
    /// [`load_from`], not by deserialization — which also keeps every test that
    /// builds a `Config` directly independent of the environment it runs in.
    #[serde(skip)]
    pub color_output: ColorOutput,
}

/// Whether colour is used, per `$NO_COLOR`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ColorOutput {
    #[default]
    Enabled,
    Disabled,
}

impl Config {
    /// The colorscheme resolved to RGB, parsed at most once per config.
    ///
    /// The renderer needs RGB for all twelve keys on every frame and used to
    /// re-parse the hex strings each time (D9).
    pub fn colors(&self) -> &ColorScheme {
        self.color_cache
            .0
            .get_or_init(|| self.colorscheme.resolve().0)
    }

    /// Whether the renderer should use the configured palette at all; `false`
    /// when `$NO_COLOR` is set to a non-empty value.
    pub fn color_enabled(&self) -> bool {
        self.color_output == ColorOutput::Enabled
    }

    /// Non-fatal problems worth telling the user about at startup, in the order
    /// they should be shown.
    ///
    /// The policy (E5/E6): anything that makes the file undecodable — malformed
    /// JSON, an unknown key, a value of the wrong type — is a hard error that
    /// stops startup with a message naming the file and the position. Anything
    /// that leaves a usable config — a colour key that does not parse, a project
    /// path that is not there right now — warns and continues, because refusing
    /// to start over an unmounted project directory would be worse than saying
    /// so. Project paths are checked lazily at the point of use, not here.
    pub fn warnings(&self) -> Vec<String> {
        self.colorscheme_errors()
            .into_iter()
            .map(|error| {
                format!(
                    "config: colorscheme.{} is not a #rrggbb color ({}); using the default",
                    error.key,
                    if error.value.is_empty() {
                        "empty".to_string()
                    } else {
                        format!("`{}`", error.value)
                    }
                )
            })
            .collect()
    }

    /// The colorscheme keys whose values are not valid `#rrggbb`.
    ///
    /// Seam for E6: resolving keeps the Rosé Pine Moon default for a bad key —
    /// which is what the renderer has always done — but the failures are
    /// reported here instead of being swallowed, so startup validation can
    /// surface them without touching the render path.
    pub fn colorscheme_errors(&self) -> Vec<ColorKeyError> {
        self.colorscheme.resolve().1
    }
}

/// The memoized result of resolving [`Config::colorscheme`].
///
/// It starts out empty and is filled on first use, which is what makes it
/// impossible to desynchronize: a `Config { colorscheme, ..Default::default() }`
/// literal inherits an *unresolved* cache rather than somebody else's palette.
/// It is also excluded from `Config`'s equality — two configs with the same
/// colorscheme are the same config whether or not either has resolved it yet.
#[derive(Debug, Default, Clone)]
pub struct ColorCache(std::sync::OnceLock<ColorScheme>);

impl PartialEq for ColorCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for ColorCache {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfiguredProject {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
}

/// A color resolved to its sRGB channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Rgb {
    const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    /// The `#rrggbb` spelling this color is written as in `config.json`.
    fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.red, self.green, self.blue)
    }
}

/// A [`ColorSchemeConfig`] with every key resolved to RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorScheme {
    pub nc: Rgb,
    pub base: Rgb,
    pub muted: Rgb,
    pub text: Rgb,
    pub love: Rgb,
    pub gold: Rgb,
    pub pine: Rgb,
    pub foam: Rgb,
    pub iris: Rgb,
    pub highlight_med: Rgb,
    pub cursor: Rgb,
    pub success: Rgb,
}

/// Rosé Pine Moon: the default palette, defined exactly once.
///
/// Every other representation is derived from this table — the `#rrggbb`
/// strings serde defaults `colorscheme` to (via the `default_moon_*` helpers
/// below) and the fallback color the renderer uses when a user key does not
/// parse. The two used to be maintained separately, as strings here and as
/// `Color::Rgb` constants in `ui.rs`, with nothing enforcing that they agreed
/// (F20).
pub const DEFAULT_COLOR_SCHEME: ColorScheme = ColorScheme {
    nc: Rgb::new(0x1f, 0x1d, 0x30),
    base: Rgb::new(0x23, 0x21, 0x36),
    muted: Rgb::new(0x6e, 0x6a, 0x86),
    text: Rgb::new(0xe0, 0xde, 0xf4),
    love: Rgb::new(0xeb, 0x6f, 0x92),
    gold: Rgb::new(0xf6, 0xc1, 0x77),
    pine: Rgb::new(0x3e, 0x8f, 0xb0),
    foam: Rgb::new(0x9c, 0xcf, 0xd8),
    iris: Rgb::new(0xc4, 0xa7, 0xe7),
    highlight_med: Rgb::new(0x44, 0x41, 0x5a),
    cursor: Rgb::new(0xff, 0xff, 0xff),
    success: Rgb::new(0x3e, 0x8f, 0x54),
};

/// A colorscheme key whose value is not a valid `#rrggbb` color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorKeyError {
    /// The `colorscheme` key as spelled in `config.json`.
    pub key: &'static str,
    pub value: String,
}

impl ColorSchemeConfig {
    /// Resolve every key to RGB, collecting the ones that failed.
    ///
    /// A key that does not parse keeps its Rosé Pine Moon default so a single
    /// typo cannot make the UI unreadable; the failure is returned rather than
    /// discarded so a caller can report it (see [`Config::colorscheme_errors`]).
    pub fn resolve(&self) -> (ColorScheme, Vec<ColorKeyError>) {
        let mut errors = Vec::new();
        let mut resolve = |key: &'static str, value: &str, fallback: Rgb| {
            parse_hex_color(value).unwrap_or_else(|| {
                errors.push(ColorKeyError {
                    key,
                    value: value.to_string(),
                });
                fallback
            })
        };

        let colors = ColorScheme {
            nc: resolve("_nc", &self.nc, DEFAULT_COLOR_SCHEME.nc),
            base: resolve("base", &self.base, DEFAULT_COLOR_SCHEME.base),
            muted: resolve("muted", &self.muted, DEFAULT_COLOR_SCHEME.muted),
            text: resolve("text", &self.text, DEFAULT_COLOR_SCHEME.text),
            love: resolve("love", &self.love, DEFAULT_COLOR_SCHEME.love),
            gold: resolve("gold", &self.gold, DEFAULT_COLOR_SCHEME.gold),
            pine: resolve("pine", &self.pine, DEFAULT_COLOR_SCHEME.pine),
            foam: resolve("foam", &self.foam, DEFAULT_COLOR_SCHEME.foam),
            iris: resolve("iris", &self.iris, DEFAULT_COLOR_SCHEME.iris),
            highlight_med: resolve(
                "highlight_med",
                &self.highlight_med,
                DEFAULT_COLOR_SCHEME.highlight_med,
            ),
            cursor: resolve("cursor", &self.cursor, DEFAULT_COLOR_SCHEME.cursor),
            success: resolve("success", &self.success, DEFAULT_COLOR_SCHEME.success),
        };

        (colors, errors)
    }
}

/// Parse `#rrggbb` (or a bare `rrggbb`), ignoring surrounding whitespace.
fn parse_hex_color(input: &str) -> Option<Rgb> {
    let input = input.trim();
    let hex = input.strip_prefix('#').unwrap_or(input);
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }

    Some(Rgb::new(
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pi_agent_command: default_pi_agent_command(),
            claude_code_command: default_claude_code_command(),
            auto_start_pi_agent: default_auto_start_pi_agent(),
            auto_start_claude_code_agent: default_auto_start_claude_code_agent(),
            auto_start_terminals: default_auto_start_terminals(),
            mouse_capture: default_mouse_capture(),
            clipboard_osc52: default_clipboard_osc52(),
            projects: Vec::new(),
            colorscheme: ColorSchemeConfig::default(),
            color_cache: ColorCache::default(),
            color_output: ColorOutput::default(),
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
        }
    }
}

/// Where the config path came from, which is what decides whether a missing
/// file is an error (F7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// The XDG default location. `mult` runs perfectly well without a config
    /// file, so a missing one means "use the defaults".
    Default,
    /// A path the user named, through `--config` or `$MULT_CONFIG_PATH`. A
    /// missing file there is a typo, not an absent config.
    Explicit,
}

/// The config file in force, and where its path came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLocation {
    pub path: PathBuf,
    pub source: ConfigSource,
}

/// Load the config the client will run with, from `location`.
///
/// This is the entry point every real load goes through: it is what applies
/// `$NO_COLOR`, which is deliberately *not* part of deserialization so a
/// `Config` built in a test never depends on the environment it runs in.
pub fn load_from(location: &ConfigLocation) -> Result<Config, ConfigError> {
    let mut config = load_from_path(&location.path, location.source)?;
    config.color_output = color_output_from(env::var_os(NO_COLOR_ENV).as_deref());
    Ok(config)
}

pub fn load_or_default() -> Result<Config, ConfigError> {
    load_from(&config_location_with_override(None))
}

/// The config location in force, given a `--config` flag (E1/F7).
pub fn config_location_with_override(flag: Option<&Path>) -> ConfigLocation {
    let (path, source) = config_path_from(
        flag,
        env::var_os(CONFIG_PATH_ENV).as_deref(),
        &config_home(),
    );
    ConfigLocation { path, source }
}

pub fn config_path() -> PathBuf {
    config_location_with_override(None).path
}

/// Pure core of [`config_location_with_override`]: both the flag and the
/// environment arrive as arguments so the precedence rule can be tested without
/// mutating a process global. `--config` beats `$MULT_CONFIG_PATH`, which beats
/// the XDG default — and only the last of the three may fall back to the
/// built-in configuration when it does not exist.
pub(crate) fn config_path_from(
    flag: Option<&Path>,
    override_path: Option<&OsStr>,
    config_home: &Path,
) -> (PathBuf, ConfigSource) {
    match (flag, override_path) {
        (Some(path), _) => (path.to_path_buf(), ConfigSource::Explicit),
        (None, Some(path)) => (PathBuf::from(path), ConfigSource::Explicit),
        (None, None) => (
            config_home.join("mult").join(CONFIG_FILE_NAME),
            ConfigSource::Default,
        ),
    }
}

/// Pure core of the `$NO_COLOR` rule: set to *any* non-empty value disables
/// colour; unset or empty leaves it on.
fn color_output_from(no_color: Option<&OsStr>) -> ColorOutput {
    match no_color {
        Some(value) if !value.is_empty() => ColorOutput::Disabled,
        _ => ColorOutput::Enabled,
    }
}

/// Why a config could not be loaded.
///
/// Both variants name the file. `fn main` used to `Debug`-print the underlying
/// `io::Error`, which produced `Custom { kind: InvalidData, error: Error("trailing
/// characters", line: 9, column: 3) }` — the position without the filename, in
/// a spelling no user should have to read (E5).
#[derive(Debug)]
pub enum ConfigError {
    /// The bytes could not be read: refused by the private-file check, wrong
    /// owner or mode, or an I/O failure. Never "not found" — that is
    /// [`ConfigError::Missing`] for a path the user named, and not an error at
    /// all for the default location.
    Read { path: PathBuf, source: io::Error },
    /// A config path the user named does not exist (F7). Distinct from `Read`
    /// so the message can say what actually happened, and distinct from "use
    /// the defaults" so a mistyped `--config` cannot silently start a session on
    /// settings the real file overrides.
    Missing { path: PathBuf },
    /// The bytes were read but do not decode: malformed JSON, an unknown key,
    /// or a value of the wrong type.
    Parse {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "config error at {}: {source}", path.display())
            }
            Self::Missing { path } => write!(
                formatter,
                "config error at {}: no such file (it was named explicitly, so the defaults are not used)",
                path.display()
            ),
            Self::Parse {
                path,
                line,
                column,
                message,
            } => write!(
                formatter,
                "config error at {}:{line}:{column}: {message}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Missing { .. } | Self::Parse { .. } => None,
        }
    }
}

impl ConfigError {
    fn parse(path: &Path, error: &serde_json::Error) -> Self {
        Self::Parse {
            path: path.to_path_buf(),
            line: error.line(),
            column: error.column(),
            message: strip_position_suffix(&error.to_string()).to_string(),
        }
    }
}

/// Drop serde_json's own ` at line N column M` tail: the position is rendered
/// separately, in the `file:line:col` form editors and tooling understand.
fn strip_position_suffix(message: &str) -> &str {
    match message.rfind(" at line ") {
        Some(index) => &message[..index],
        None => message,
    }
}

/// Upper bound on `config.json`. It is a small hand-written object; a config
/// larger than this is a mistake or a hostile substitution, and reading it
/// unboundedly is how a `/dev/zero` symlink at the path used to hang startup.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Read the config with the guarantees a file this powerful needs.
///
/// `pi_agent_command` and `claude_code_command` are run through `$SHELL -lc`
/// and auto-started by default, so whoever controls these bytes controls a
/// process running as the user, with no keystroke required. The path itself is
/// `$MULT_CONFIG_PATH`-overridable, so it proves nothing on its own:
/// [`mult_protocol::read_private_file`] is what establishes that the file is a
/// regular file, not reached through a symlink, owned by this user, not
/// writable by anyone else, and bounded. Anything that is not a clean read is
/// refused rather than trusted.
///
/// A missing file means "use the defaults" only at the *default* location
/// (F7). `mult --config ~/.config/mult/confg.json` — one letter out — used to
/// start with exit 0 on the built-in configuration, which auto-runs the default
/// `pi` and `claude` command lines even when the real config turned them off.
/// The same hole was reachable through `$MULT_CONFIG_PATH`.
fn load_from_path(path: &Path, source: ConfigSource) -> Result<Config, ConfigError> {
    match mult_protocol::read_private_file(path, MAX_CONFIG_BYTES) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|error| ConfigError::parse(path, &error))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => match source {
            ConfigSource::Default => Ok(Config::default()),
            ConfigSource::Explicit => Err(ConfigError::Missing {
                path: path.to_path_buf(),
            }),
        },
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
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

fn default_clipboard_osc52() -> bool {
    true
}

// The twelve `colorscheme` defaults are spelled from `DEFAULT_COLOR_SCHEME`
// rather than repeated as literals, so the strings a fresh `config.json`
// documents and the colors the renderer draws cannot disagree.

fn default_moon_nc() -> String {
    DEFAULT_COLOR_SCHEME.nc.to_hex()
}

fn default_moon_base() -> String {
    DEFAULT_COLOR_SCHEME.base.to_hex()
}

fn default_moon_muted() -> String {
    DEFAULT_COLOR_SCHEME.muted.to_hex()
}

fn default_moon_text() -> String {
    DEFAULT_COLOR_SCHEME.text.to_hex()
}

fn default_moon_love() -> String {
    DEFAULT_COLOR_SCHEME.love.to_hex()
}

fn default_moon_gold() -> String {
    DEFAULT_COLOR_SCHEME.gold.to_hex()
}

fn default_moon_pine() -> String {
    DEFAULT_COLOR_SCHEME.pine.to_hex()
}

fn default_moon_foam() -> String {
    DEFAULT_COLOR_SCHEME.foam.to_hex()
}

fn default_moon_iris() -> String {
    DEFAULT_COLOR_SCHEME.iris.to_hex()
}

fn default_moon_highlight_med() -> String {
    DEFAULT_COLOR_SCHEME.highlight_med.to_hex()
}

fn default_cursor() -> String {
    DEFAULT_COLOR_SCHEME.cursor.to_hex()
}

fn default_success() -> String {
    DEFAULT_COLOR_SCHEME.success.to_hex()
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
        assert!(config.clipboard_osc52);
        assert!(config.projects.is_empty());
        assert_eq!(config.colorscheme.base, "#232136");
        assert_eq!(config.colorscheme.nc, "#1f1d30");
    }

    #[test]
    fn default_colorscheme_strings_are_the_canonical_rgb_table() {
        // The two representations of Rosé Pine Moon used to be maintained by
        // hand in two files (F20). The strings are now spelled from the table,
        // so this fails if the table stops parsing back to itself, and the
        // pinned values below fail if a key changes by accident rather than as
        // a deliberate theme edit.
        let strings = ColorSchemeConfig::default();
        let (colors, errors) = strings.resolve();

        assert!(errors.is_empty());
        assert_eq!(colors, DEFAULT_COLOR_SCHEME);
        assert_eq!(strings.nc, "#1f1d30");
        assert_eq!(DEFAULT_COLOR_SCHEME.nc, Rgb::new(31, 29, 48));
        assert_eq!(strings.base, "#232136");
        assert_eq!(DEFAULT_COLOR_SCHEME.base, Rgb::new(35, 33, 54));
        assert_eq!(strings.highlight_med, "#44415a");
        assert_eq!(strings.cursor, "#ffffff");
        assert_eq!(strings.success, "#3e8f54");
    }

    #[test]
    fn colors_follow_the_colorscheme_through_a_struct_update() {
        // The resolved palette is a memo, and the memo has to belong to the
        // colorscheme it was built from — including when a config is assembled
        // by updating another one, where an eagerly-filled cache would hand
        // back the palette of the config that was updated *from*.
        let config = Config {
            colorscheme: ColorSchemeConfig {
                base: "#010203".to_string(),
                ..ColorSchemeConfig::default()
            },
            ..Config::default()
        };

        assert_eq!(config.colors().base, Rgb::new(1, 2, 3));
        assert_eq!(config.colors().text, DEFAULT_COLOR_SCHEME.text);
        // Repeated reads are the same memo, not a re-parse with a different answer.
        assert_eq!(config.colors(), config.colors());
    }

    #[test]
    fn unparsable_colors_keep_the_default_and_are_reported() {
        let mut config = Config::default();
        config.colorscheme.love = "#gggggg".to_string();
        config.colorscheme.gold = String::new();

        let errors = config.colorscheme_errors();

        assert_eq!(config.colors().love, DEFAULT_COLOR_SCHEME.love);
        assert_eq!(config.colors().gold, DEFAULT_COLOR_SCHEME.gold);
        assert_eq!(
            errors,
            vec![
                ColorKeyError {
                    key: "love",
                    value: "#gggggg".to_string()
                },
                ColorKeyError {
                    key: "gold",
                    value: String::new()
                },
            ]
        );
    }

    #[test]
    fn hex_colors_accept_a_bare_or_prefixed_value_and_reject_the_rest() {
        assert_eq!(parse_hex_color("#0a0B0c"), Some(Rgb::new(10, 11, 12)));
        assert_eq!(parse_hex_color("  0a0b0c  "), Some(Rgb::new(10, 11, 12)));
        assert_eq!(parse_hex_color("#0a0b0"), None);
        assert_eq!(parse_hex_color("#0a0b0cc"), None);
        assert_eq!(parse_hex_color("#0a0b0g"), None);
        assert_eq!(parse_hex_color(""), None);
    }

    #[test]
    fn config_path_prefers_the_flag_then_the_env_then_the_config_home() {
        let config_home = Path::new("/home/example/.config");

        // Both overrides are paths the user named, so a missing file there is an
        // error rather than a silent fall back to the defaults (F7).
        assert_eq!(
            config_path_from(
                Some(Path::new("/tmp/from-flag.json")),
                Some(OsStr::new("/tmp/mult-test-config.json")),
                config_home
            ),
            (PathBuf::from("/tmp/from-flag.json"), ConfigSource::Explicit)
        );
        assert_eq!(
            config_path_from(
                None,
                Some(OsStr::new("/tmp/mult-test-config.json")),
                config_home
            ),
            (
                PathBuf::from("/tmp/mult-test-config.json"),
                ConfigSource::Explicit
            )
        );
        assert_eq!(
            config_path_from(None, None, config_home),
            (
                PathBuf::from("/home/example/.config/mult/config.json"),
                ConfigSource::Default
            )
        );
    }

    #[test]
    fn no_color_disables_color_for_any_non_empty_value() {
        assert_eq!(color_output_from(None), ColorOutput::Enabled);
        assert_eq!(
            color_output_from(Some(OsStr::new(""))),
            ColorOutput::Enabled
        );
        assert_eq!(
            color_output_from(Some(OsStr::new("1"))),
            ColorOutput::Disabled
        );
        // Per no-color.org the *value* is irrelevant; only emptiness is.
        assert_eq!(
            color_output_from(Some(OsStr::new("0"))),
            ColorOutput::Disabled
        );
        assert!(Config::default().color_enabled());
    }

    #[test]
    fn malformed_config_json_is_an_error_not_a_silent_default() {
        // The E5 policy: a config that does not decode stops startup with a
        // message naming the file *and* the position, rather than dying on a
        // `Debug`-printed `io::Error` or quietly running with the defaults —
        // which for this file would mean auto-starting a different command.
        let path = unique_temp_file();
        fs::write(&path, "{\n  \"mouse_capture\": true,\n}\n").expect("write config");

        let error =
            load_from_path(&path, ConfigSource::Default).expect_err("malformed config must fail");

        let ConfigError::Parse {
            line,
            column,
            ref message,
            ..
        } = error
        else {
            panic!("expected a parse error, got {error:?}");
        };
        assert_eq!((line, column), (3, 1));
        assert!(!message.contains("at line"), "{message}");
        let rendered = error.to_string();
        assert!(
            rendered.starts_with(&format!("config error at {}:3:1: ", path.display())),
            "{rendered}"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn unknown_config_keys_are_reported() {
        // `auto_start_terminal` is `auto_start_terminals` minus one letter: the
        // exact typo E6 names. It used to deserialize fine and do nothing.
        let path = unique_temp_file();
        fs::write(&path, r#"{"auto_start_terminal":false}"#).expect("write config");

        let error =
            load_from_path(&path, ConfigSource::Default).expect_err("an unknown key must fail");

        let rendered = error.to_string();
        assert!(rendered.contains("auto_start_terminal"), "{rendered}");
        assert!(rendered.contains(&path.display().to_string()), "{rendered}");
        assert!(rendered.contains("unknown field"), "{rendered}");
        // The same rule applies one level down, inside `colorscheme`.
        fs::write(&path, r##"{"colorscheme":{"iriss":"#c4a7e7"}}"##).expect("write config");
        let nested = load_from_path(&path, ConfigSource::Default)
            .expect_err("an unknown colorscheme key must fail")
            .to_string();
        assert!(nested.contains("iriss"), "{nested}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn invalid_color_strings_fall_back_to_the_default_palette_and_report() {
        // The other half of the policy: a bad *value* in a decodable file warns
        // and keeps running, because one mistyped hex digit must not be able to
        // lock the user out of their sessions.
        let path = unique_temp_file();
        fs::write(
            &path,
            r##"{"colorscheme":{"love":"#gggggg","gold":"","iris":"#c4a7e7"}}"##,
        )
        .expect("write config");

        let config = load_from_path(&path, ConfigSource::Default)
            .expect("a bad color must not fail the load");

        assert_eq!(config.colors().love, DEFAULT_COLOR_SCHEME.love);
        assert_eq!(config.colors().gold, DEFAULT_COLOR_SCHEME.gold);
        assert_eq!(config.colors().iris, Rgb::new(0xc4, 0xa7, 0xe7));
        let warnings = config.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].contains("colorscheme.love") && warnings[0].contains("#gggggg"),
            "{warnings:?}"
        );
        assert!(
            warnings[1].contains("colorscheme.gold") && warnings[1].contains("empty"),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().all(|warning| warning.contains("default")),
            "{warnings:?}"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_valid_config_reports_no_warnings() {
        assert!(Config::default().warnings().is_empty());
    }

    #[test]
    fn config_loads_pi_agent_command_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"pi_agent_command":"pi -c"}"#).expect("write config");

        let config = load_from_path(&path, ConfigSource::Default).expect("load config");

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

        let config = load_from_path(&path, ConfigSource::Default).expect("load config");

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

        let config = load_from_path(&path, ConfigSource::Default).expect("load config");

        assert_eq!(config.pi_agent_command, "pi");
        assert!(!config.auto_start_pi_agent);
        assert!(!config.auto_start_claude_code_agent);
        assert!(!config.auto_start_terminals);
    }

    #[test]
    fn config_loads_claude_code_command_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"claude_code_command":"claude --resume"}"#).expect("write config");

        let config = load_from_path(&path, ConfigSource::Default).expect("load config");

        // The pi command keeps its default while the cc command is overridden.
        assert_eq!(config.pi_agent_command, "pi");
        assert_eq!(config.claude_code_command, "claude --resume");
        assert!(config.auto_start_claude_code_agent);
    }

    #[test]
    fn config_loads_clipboard_flag_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"clipboard_osc52":false}"#).expect("write config");

        let config = load_from_path(&path, ConfigSource::Default).expect("load config");

        assert!(!config.clipboard_osc52);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_loads_mouse_capture_flag_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"mouse_capture":false}"#).expect("write config");

        let config = load_from_path(&path, ConfigSource::Default).expect("load config");

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

        let config = load_from_path(&path, ConfigSource::Default).expect("load config");

        assert_eq!(config.colorscheme.nc, "#000001");
        assert_eq!(config.colorscheme.text, "#ffffff");
        assert_eq!(config.colorscheme.base, "#232136");
    }

    #[cfg(unix)]
    #[test]
    fn config_behind_a_symlink_is_refused_rather_than_followed() {
        // The planted target holds a command line that would be shell-evaluated
        // and auto-started. Following the link would run it; refusing does not.
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir).expect("create temp dir");
        let target = dir.join("planted.json");
        fs::write(&target, r#"{"pi_agent_command":"touch /tmp/pwned"}"#).expect("write target");
        let link = dir.join("config.json");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let error = load_from_path(&link, ConfigSource::Default)
            .expect_err("a symlinked config must be refused");

        // Never silently downgraded to "missing, use defaults": the difference
        // between "no config" and "a config someone else can aim" must be loud.
        let ConfigError::Read { ref source, .. } = error else {
            panic!("expected a read error, got {error:?}");
        };
        assert_ne!(source.kind(), io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains(&link.display().to_string()),
            "{error}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_config_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let path = unique_temp_file();
        fs::write(&path, r#"{"pi_agent_command":"touch /tmp/pwned"}"#).expect("write config");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("chmod");

        let error = load_from_path(&path, ConfigSource::Default)
            .expect_err("a world-writable config must be refused");

        let ConfigError::Read { ref source, .. } = error else {
            panic!("expected a read error, got {error:?}");
        };
        assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_config_uses_defaults() {
        let config = load_from_path(&unique_temp_file(), ConfigSource::Default)
            .expect("load missing config");

        assert_eq!(config, Config::default());
    }

    /// F7: a path the user typed is not a place where "absent" can mean "use
    /// the built-in configuration".
    ///
    /// `mult --config ~/.config/mult/confg.json` used to start with exit 0 on
    /// the defaults — which auto-run `pi` and `claude` — even though the real
    /// config had `auto_start_pi_agent: false`. `$MULT_CONFIG_PATH` had the same
    /// hole.
    #[test]
    fn a_missing_config_named_by_the_user_is_an_error_not_the_defaults() {
        let path = unique_temp_file();

        let error = load_from_path(&path, ConfigSource::Explicit)
            .expect_err("an explicitly named config must exist");

        assert!(matches!(error, ConfigError::Missing { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        // The defaults it would have fallen back to are exactly the ones that
        // auto-start a shell command line.
        assert!(Config::default().auto_start_pi_agent);
    }

    /// The other half of F7: `load_from` carries the source through, so the same
    /// missing file is fatal for `--config` and fine for the default location.
    #[test]
    fn load_from_honours_the_source_of_the_path() {
        let path = unique_temp_file();

        assert!(load_from(&ConfigLocation {
            path: path.clone(),
            source: ConfigSource::Default,
        })
        .is_ok());
        assert!(load_from(&ConfigLocation {
            path,
            source: ConfigSource::Explicit,
        })
        .is_err());
    }

    fn unique_temp_file() -> PathBuf {
        unique_temp_dir().with_extension("json")
    }

    fn unique_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("mult-config-test-{unique}-{}", std::process::id()))
    }
}
