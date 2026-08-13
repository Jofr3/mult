use std::{
    env,
    ffi::OsStr,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde::{
    de::{self, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};

use crate::{
    paths,
    storage::{read_private_file, SecureDirectory, CONFIG_DIRECTORY},
    ui::Palette,
};

const CONFIG_PATH_ENV: &str = "MULT_CONFIG_PATH";
const CONFIG_FILE_NAME: &str = "config.json";
/// Upper bound on a config file read into memory. Config is JSON a human
/// writes; anything near this is not one.
const MAX_CONFIG_FILE_BYTES: usize = 1024 * 1024;

/// The parsed `config.json`.
///
/// Two policies, applied consistently (documented in `docs/CONFIG.md`):
///
/// - anything that means the file cannot be understood is a **startup error**:
///   malformed JSON, a value of the wrong type, and — with
///   `deny_unknown_fields` — a key `mult` does not know. A typo used to be
///   accepted and do nothing, which is indistinguishable from the feature not
///   working (E6).
/// - anything that has a safe, obvious fallback is a **warning**: a colour that
///   does not parse keeps the built-in default, and a `projects` entry pointing
///   somewhere that does not exist is still offered. Both are collected into
///   [`Config::warnings`] rather than swallowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_pi_agent_command")]
    pub pi_agent_command: String,
    #[serde(default = "default_claude_code_command")]
    pub claude_code_command: String,
    /// The file manager `Ctrl+n` opens in the selected workspace's root
    /// directory. Run through the login shell, like the agent commands.
    #[serde(default = "default_file_manager_command")]
    pub file_manager_command: String,
    /// The editor `Ctrl+e` opens in the selected workspace's root directory.
    /// Run through the login shell, like the agent commands.
    ///
    /// Empty — the default — means "whatever this user's editor is": see
    /// [`Config::resolved_editor_command`]. Set it to pin one editor for
    /// `mult` regardless of the environment it was started from.
    #[serde(default = "default_editor_command")]
    pub editor_command: String,
    #[serde(default = "default_auto_start_pi_agent")]
    pub auto_start_pi_agent: bool,
    #[serde(default = "default_auto_start_claude_code_agent")]
    pub auto_start_claude_code_agent: bool,
    #[serde(default = "default_auto_start_terminals")]
    pub auto_start_terminals: bool,
    #[serde(default = "default_mouse_capture")]
    pub mouse_capture: bool,
    /// Whether a text selection is copied to the system clipboard with OSC 52.
    ///
    /// Defaults to on, which is what `mult` has always done. Turning it off
    /// stops `mult` from ever writing an OSC 52 sequence: the escape carries
    /// the selected text to whatever is on the other end of the terminal (an
    /// SSH client, a multiplexer, a terminal that logs sequences), which is not
    /// always where the user wants pane contents to go.
    #[serde(default = "default_clipboard_osc52")]
    pub clipboard_osc52: bool,
    #[serde(default)]
    pub projects: Vec<ConfiguredProject>,
    #[serde(default)]
    pub colorscheme: ColorSchemeConfig,
    /// Non-fatal problems found while loading, in file order. Derived from the
    /// keys above rather than read from the file, so it is skipped by serde and
    /// ignored by equality.
    ///
    /// Read it through [`Config::warnings`]. It is public only so a `Config`
    /// stays constructible with a struct literal outside this crate, which the
    /// binary's tests rely on; loading is the only thing that should write it.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl Config {
    /// The parsed color scheme, memoized on the [`ColorSchemeConfig`] it comes
    /// from. See [`ColorSchemeConfig::palette`].
    pub fn palette(&self) -> Palette {
        self.colorscheme.palette()
    }

    /// The editor command `Ctrl+e` runs, with an unset `editor_command`
    /// resolved from the environment.
    ///
    /// "Preferred editor" is a thing the user already told their system once,
    /// so the default asks the environment rather than a `mult` key: `$VISUAL`
    /// first (it is the one that means "a full-screen editor on this
    /// terminal"), then `$EDITOR`, then `vi`, which POSIX requires to exist.
    /// The fallback chain never yields an empty command, so `Ctrl+e` always has
    /// something to run.
    pub fn resolved_editor_command(&self) -> String {
        resolve_editor_command(&self.editor_command, |name| env::var(name).ok())
    }

    /// Problems that did not stop startup, each a complete sentence naming the
    /// config file.
    ///
    /// `main` prints these to stderr today. They are exposed as plain strings
    /// so the in-app status surface (E2) can show them without knowing anything
    /// about colours or project paths.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Collects the value-level warnings for a config just read from `path`.
    fn collect_warnings(&mut self, path: &Path) {
        let file = path.display();
        let mut warnings = Vec::new();

        for issue in Palette::from_colorscheme_reporting(&self.colorscheme).1 {
            warnings.push(format!(
                "{file}: colorscheme.{} is not a #rrggbb color ({:?}); keeping the built-in default",
                issue.key, issue.value
            ));
        }

        for (index, project) in self.projects.iter().enumerate() {
            if project.name.trim().is_empty() {
                warnings.push(format!("{file}: projects[{index}] has an empty name"));
            }
            if project.path.as_os_str().is_empty() {
                warnings.push(format!("{file}: projects[{index}] has an empty path"));
                continue;
            }
            // Checked, not enforced: a project directory may legitimately be on
            // a filesystem that is not mounted yet, and refusing to start over a
            // shortcut nobody pressed would be absurd. The shortcut stays in the
            // list either way and fails when it is opened.
            let expanded = project.expanded_path();
            if !expanded.is_dir() {
                warnings.push(format!(
                    "{file}: project {:?} points at {} which is not a directory; the shortcut is still offered",
                    project.name,
                    expanded.display()
                ));
            }
        }

        self.warnings = warnings;
    }
}

// Equality is over the configured values only. `warnings` is derived from them
// plus the state of the filesystem at load time, so two configs with the same
// contents stay equal whether or not either was read from disk — the same rule
// the memoized palette follows.
impl PartialEq for Config {
    fn eq(&self, other: &Self) -> bool {
        self.pi_agent_command == other.pi_agent_command
            && self.claude_code_command == other.claude_code_command
            && self.file_manager_command == other.file_manager_command
            && self.editor_command == other.editor_command
            && self.auto_start_pi_agent == other.auto_start_pi_agent
            && self.auto_start_claude_code_agent == other.auto_start_claude_code_agent
            && self.auto_start_terminals == other.auto_start_terminals
            && self.mouse_capture == other.mouse_capture
            && self.clipboard_osc52 == other.clipboard_osc52
            && self.projects == other.projects
            && self.colorscheme == other.colorscheme
    }
}

impl Eq for Config {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfiguredProject {
    pub name: String,
    pub path: PathBuf,
}

impl ConfiguredProject {
    /// `path` with a leading `~` expanded from `$HOME`, as the open-workspace
    /// prompt expands it before importing the workspace.
    ///
    /// `app::expand_path` is the twin of this and should collapse onto it when
    /// F20 folds the duplicated helpers together; it is duplicated rather than
    /// shared today because `app` is not a dependency of `config`.
    pub fn expanded_path(&self) -> PathBuf {
        let Some(text) = self.path.to_str() else {
            return self.path.clone();
        };
        if text == "~" {
            return env::var_os("HOME").map_or_else(|| self.path.clone(), PathBuf::from);
        }
        match (text.strip_prefix("~/"), env::var_os("HOME")) {
            (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
            _ => self.path.clone(),
        }
    }
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
            file_manager_command: default_file_manager_command(),
            editor_command: default_editor_command(),
            auto_start_pi_agent: default_auto_start_pi_agent(),
            auto_start_claude_code_agent: default_auto_start_claude_code_agent(),
            auto_start_terminals: default_auto_start_terminals(),
            mouse_capture: default_mouse_capture(),
            clipboard_osc52: default_clipboard_osc52(),
            projects: Vec::new(),
            colorscheme: ColorSchemeConfig::default(),
            warnings: Vec::new(),
        }
    }
}

/// A project is written either as `{"name": …, "path": …}` or as the pair
/// `["name", "path"]`.
///
/// Hand-written rather than an `#[serde(untagged)]` enum: untagged buffers the
/// value and reports every failure as "data did not match any variant", losing
/// both the offending key and the line it was on. Streaming it keeps
/// `deny_unknown_fields`-quality messages *and* serde_json's position, so a
/// mistyped `"pathh"` reads like every other config error (E6).
impl<'de> Deserialize<'de> for ConfiguredProject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ProjectVisitor;

        impl<'de> Visitor<'de> for ProjectVisitor {
            type Value = ConfiguredProject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .write_str("a project object with `name` and `path`, or a [name, path] pair")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<ConfiguredProject, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let name = sequence
                    .next_element::<String>()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let path = sequence
                    .next_element::<PathBuf>()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(3, &self));
                }
                Ok(ConfiguredProject { name, path })
            }

            fn visit_map<A>(self, mut map: A) -> Result<ConfiguredProject, A::Error>
            where
                A: MapAccess<'de>,
            {
                const FIELDS: &[&str] = &["name", "path"];
                let mut name: Option<String> = None;
                let mut path: Option<PathBuf> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            if name.is_some() {
                                return Err(de::Error::duplicate_field("name"));
                            }
                            name = Some(map.next_value()?);
                        }
                        "path" => {
                            if path.is_some() {
                                return Err(de::Error::duplicate_field("path"));
                            }
                            path = Some(map.next_value()?);
                        }
                        unknown => return Err(de::Error::unknown_field(unknown, FIELDS)),
                    }
                }

                Ok(ConfiguredProject {
                    name: name.ok_or_else(|| de::Error::missing_field("name"))?,
                    path: path.ok_or_else(|| de::Error::missing_field("path"))?,
                })
            }
        }

        deserializer.deserialize_any(ProjectVisitor)
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

/// Loads the config, with `flag` being whatever `--config` carried.
pub fn load_or_default(flag: Option<&Path>) -> io::Result<Config> {
    load_from_path(&resolve_config_path(flag)?)
}

pub fn resolve_config_path(flag: Option<&Path>) -> io::Result<PathBuf> {
    config_path_from(
        flag,
        env::var_os(CONFIG_PATH_ENV).as_deref(),
        paths::config_home,
    )
}

/// The config-path policy as a pure function of its inputs: `--config`, then
/// `$MULT_CONFIG_PATH`, then `<config home>/mult/config.json`.
///
/// `$MULT_CONFIG_PATH` is a process global, so the test that used to cover this
/// either mutated it — racing every sibling test — or branched on whatever the
/// developer's environment happened to hold and asserted nothing useful (G7).
/// The wrapper above reads the environment once and this decides; the test
/// drives this directly. `config_home` stays lazy because an explicit override
/// must keep working on a machine with no resolvable configuration home.
fn config_path_from(
    flag: Option<&Path>,
    environment: Option<&OsStr>,
    config_home: impl FnOnce() -> io::Result<PathBuf>,
) -> io::Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path.to_path_buf());
    }
    match environment {
        Some(path) => Ok(PathBuf::from(path)),
        None => Ok(config_home()?.join("mult").join(CONFIG_FILE_NAME)),
    }
}

/// Display-oriented compatibility helper. Loading uses [`resolve_config_path`]
/// and returns an error instead of ever selecting the current directory.
///
/// Note this cannot see `--config`: it is called from the renderer, which is
/// handed a [`Config`] and not the command line. It therefore names the path a
/// *default* invocation would read.
pub fn config_path() -> PathBuf {
    resolve_config_path(None).unwrap_or_else(|_| PathBuf::from("<configuration path unavailable>"))
}

/// Read the config with the same discipline as state, and for a sharper reason:
/// `pi_agent_command` and `claude_code_command` are handed to `$SHELL -lc` and
/// auto-started by default, so *whoever controls these bytes runs code as this
/// user without a keystroke*. Two environment variables steer the path there
/// (`$MULT_CONFIG_PATH`, and `$XDG_CONFIG_HOME` via [`paths::config_home`],
/// which accepts any absolute value), so the path is treated as untrusted.
///
/// The invariant enforced is *not* "no symlink was traversed" — it is **the
/// bytes came from a file only this user can write, in a directory only this
/// user can write**. So the path is resolved through symlinks first and the
/// checks are applied to what it resolved *to* (C14):
///
/// - resolution is an untrusted hint. It picks the path; it grants nothing.
/// - the resolved path contains no symlinks by construction, so the existing
///   fd-walk still opens every component with `O_NOFOLLOW` — a component
///   swapped for a link mid-walk is still refused — and the final parent must
///   be owned by this user and not group/other-writable (that ownership check
///   is what makes a redirected `$XDG_CONFIG_HOME` useless to an attacker —
///   S7).
/// - the file itself must be a regular, singly-linked, owner-only file, and is
///   read under a size cap.
///
/// Racing the resolution therefore buys an attacker nothing they did not
/// already have: the redirected target must still be *this user's* own
/// `0600`, single-link file inside a directory only this user can write, which
/// is a strictly narrower position than the write access to that directory
/// they would need to plant the link in the first place.
///
/// A missing file — a missing config directory, or a link that points nowhere
/// — still means "use defaults". Anything else fails loudly rather than
/// silently running with defaults, because a rejected config is a signal, not
/// a fallback.
fn load_from_path(path: &Path) -> io::Result<Config> {
    // Untrusted: this only decides *which* path the checks below are applied
    // to. `canonicalize` reports `NotFound` for a missing file and for a
    // dangling link alike, which is the same "use defaults" as a missing
    // config directory.
    let resolved = match fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(describe_read_failure(path, error)),
    };

    let directory =
        match SecureDirectory::open_parent_for(&resolved, false, false, CONFIG_DIRECTORY) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(error) => return Err(describe_read_failure(path, error)),
        };
    let name = resolved.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("config path must name a file: {}", path.display()),
        )
    })?;

    let bytes = read_private_file(
        &directory,
        name,
        &describe_config_file(path, &resolved),
        MAX_CONFIG_FILE_BYTES,
    )
    .map_err(|error| describe_read_failure(path, error))?;

    match bytes {
        Some(bytes) => {
            // Parse failures and warnings name the *resolved* file: that is the
            // one to open in an editor, and for a linked config it is not the
            // path the user typed.
            let mut config: Config = serde_json::from_slice(&bytes)
                .map_err(|error| describe_parse_failure(&resolved, error))?;
            config.collect_warnings(&resolved);
            Ok(config)
        }
        None => Ok(Config::default()),
    }
}

/// Names the file the ownership and mode checks actually ran against.
///
/// For a linked config the path the user typed and the file that has to be
/// `chmod`ed are different files, and only naming one of them sends them to
/// the wrong place.
fn describe_config_file(path: &Path, resolved: &Path) -> String {
    if resolved == path {
        format!("config file {}", path.display())
    } else {
        format!(
            "config file {} (resolved to {})",
            path.display(),
            resolved.display()
        )
    }
}

/// Turns a `serde_json` failure into `config error at <path>:<line>:<col>: …`.
///
/// The user used to get a `Debug` dump with no filename in it at all (E5).
/// serde carries the position separately *and* repeats it at the end of the
/// message, so the tail is stripped rather than printed twice.
fn describe_parse_failure(path: &Path, error: serde_json::Error) -> io::Error {
    let message = error.to_string();
    let message = match message.find(" at line ") {
        Some(cut) => &message[..cut],
        None => message.as_str(),
    };
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "config error at {}:{}:{}: {message}",
            path.display(),
            error.line(),
            error.column()
        ),
    )
}

/// Names the config file on the failures that happen *below* `mult`, inside
/// `canonicalize` or the `O_NOFOLLOW` opens.
///
/// These used to arrive as a bare `Os { code: 40, kind: FilesystemLoop, … }`
/// naming neither the file nor the reason (E5). Since C14 resolves the path
/// through symlinks, a linked config is no longer one of them — `ELOOP` now
/// means a genuine link *cycle*, and `ENOTDIR` a path routed through something
/// that is not a directory.
fn describe_read_failure(path: &Path, error: io::Error) -> io::Error {
    let cause = match error.raw_os_error() {
        Some(libc::ELOOP) => "too many levels of symbolic links",
        // From `canonicalize` when a component is a plain file, and from the
        // fd-walk's `O_NOFOLLOW|O_DIRECTORY` open if a component is swapped
        // for a link after resolution.
        Some(libc::ENOTDIR) => "a component of the path is not a directory",
        _ => return error,
    };

    io::Error::new(
        error.kind(),
        format!("config error at {}: {cause}", path.display()),
    )
}

fn default_pi_agent_command() -> String {
    "pi".to_string()
}

fn default_claude_code_command() -> String {
    "claude".to_string()
}

fn default_file_manager_command() -> String {
    "yazi".to_string()
}

/// Empty on purpose: the editor is resolved from the environment unless the
/// user pins one. See [`Config::resolved_editor_command`].
fn default_editor_command() -> String {
    String::new()
}

/// The `$VISUAL` → `$EDITOR` → `vi` chain, with the environment injected so it
/// can be tested without mutating the process's own.
///
/// A variable that is set but blank (or only spaces) is treated as unset: it
/// would otherwise reach the login shell as an empty command line, which exits
/// at once and leaves a pane that never says why.
fn resolve_editor_command(configured: &str, env: impl Fn(&str) -> Option<String>) -> String {
    let non_blank = |value: String| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    };

    non_blank(configured.to_string())
        .or_else(|| env("VISUAL").and_then(non_blank))
        .or_else(|| env("EDITOR").and_then(non_blank))
        .unwrap_or_else(|| "vi".to_string())
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn config_defaults_to_pi_command() {
        let config = Config::default();

        assert_eq!(config.pi_agent_command, "pi");
        assert_eq!(config.claude_code_command, "claude");
        assert_eq!(config.file_manager_command, "yazi");
        assert_eq!(config.editor_command, "");
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
    fn config_path_uses_config_home_or_override() {
        let from_home = config_path_from(None, None, || Ok(PathBuf::from("/xdg-config")))
            .expect("a resolvable config home yields a path");
        assert_eq!(from_home, Path::new("/xdg-config/mult/config.json"));

        let overridden = config_path_from(None, Some(OsStr::new("/elsewhere/mult.json")), || {
            panic!("an override must not consult the configuration home")
        })
        .expect("an override is used verbatim");
        assert_eq!(overridden, Path::new("/elsewhere/mult.json"));
    }

    /// E1's precedence rule, at the seam that decides it: `--config` outranks
    /// `$MULT_CONFIG_PATH`, which outranks the configuration home.
    #[test]
    fn the_config_flag_outranks_the_environment_and_the_default() {
        let from_flag = config_path_from(
            Some(Path::new("/flag/config.json")),
            Some(OsStr::new("/environment/config.json")),
            || panic!("a flag must not consult the configuration home"),
        )
        .expect("the flag is used verbatim");
        assert_eq!(from_flag, Path::new("/flag/config.json"));

        let from_environment =
            config_path_from(None, Some(OsStr::new("/environment/config.json")), || {
                panic!("the environment must not consult the configuration home")
            })
            .expect("the environment is used verbatim");
        assert_eq!(from_environment, Path::new("/environment/config.json"));
    }

    #[test]
    fn config_path_reports_an_unresolvable_home_instead_of_guessing() {
        let error = config_path_from(None, None, || {
            Err(io::Error::new(io::ErrorKind::NotFound, "no home"))
        })
        .expect_err("without a config home there is no path to return");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("no home"));
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
    fn config_loads_file_manager_command_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"file_manager_command":"lf"}"#).expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert_eq!(config.file_manager_command, "lf");
    }

    #[test]
    fn config_loads_editor_command_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"editor_command":"hx"}"#).expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert_eq!(config.editor_command, "hx");
        assert_eq!(config.resolved_editor_command(), "hx");
    }

    #[test]
    fn editor_command_prefers_the_config_then_visual_then_editor() {
        fn env(
            visual: Option<&'static str>,
            editor: Option<&'static str>,
        ) -> impl Fn(&str) -> Option<String> {
            move |name: &str| match name {
                "VISUAL" => visual.map(str::to_string),
                "EDITOR" => editor.map(str::to_string),
                _ => None,
            }
        }

        // A configured editor wins over both variables.
        assert_eq!(
            resolve_editor_command("hx", env(Some("nvim"), Some("vim"))),
            "hx"
        );
        assert_eq!(
            resolve_editor_command("", env(Some("nvim"), Some("vim"))),
            "nvim"
        );
        assert_eq!(resolve_editor_command("", env(None, Some("vim"))), "vim");
    }

    #[test]
    fn editor_command_falls_back_to_vi_when_nothing_is_set() {
        // A blank variable counts as unset: `$SHELL -lc ""` would exit at once
        // and leave a pane with nothing to say for itself.
        assert_eq!(
            resolve_editor_command("  ", |name| match name {
                "VISUAL" => Some("   ".to_string()),
                _ => None,
            }),
            "vi"
        );
        assert_eq!(resolve_editor_command("", |_| None), "vi");
    }

    #[test]
    fn config_loads_mouse_capture_flag_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"mouse_capture":false}"#).expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert!(!config.mouse_capture);
        assert!(config.clipboard_osc52, "unrelated keys keep their defaults");
    }

    #[test]
    fn config_loads_clipboard_opt_out_from_json() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"clipboard_osc52":false}"#).expect("write config");

        let config = load_from_path(&path).expect("load config");

        assert!(!config.clipboard_osc52);
        assert!(config.mouse_capture);
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

    #[test]
    fn missing_config_directory_uses_defaults() {
        let path = unique_temp_dir().join("absent").join(CONFIG_FILE_NAME);

        let config = load_from_path(&path).expect("load config with no directory");

        assert_eq!(config, Config::default());
    }

    /// G13: a config that is not JSON must fail loudly. Silently starting on
    /// the defaults would tell the user their file "does not work" with no way
    /// to find out why.
    #[test]
    fn malformed_config_json_is_an_error_not_a_silent_default() {
        let path = unique_temp_file();
        fs::write(&path, "{\n  \"pi_agent_command\": \"pi\",\n}\n").expect("write config");

        let error = load_from_path(&path).expect_err("malformed JSON must be refused");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let message = error.to_string();
        assert!(
            message.starts_with(&format!("config error at {}:3:1: ", path.display())),
            "the file and position must lead the message: {message}"
        );
        // serde repeats the position at the end of its own message; printing it
        // twice is noise.
        assert!(!message.contains("at line 3 column 1"), "{message}");
        assert!(message.contains("trailing comma"), "{message}");
    }

    /// G13/E6: `auto_start_terminal` (no `s`) used to be accepted and do
    /// nothing, which is indistinguishable from the setting not working.
    #[test]
    fn unknown_config_keys_are_reported() {
        let path = unique_temp_file();
        fs::write(&path, "{\n  \"auto_start_terminal\": false\n}\n").expect("write config");

        let error = load_from_path(&path).expect_err("an unknown key must be refused");

        let message = error.to_string();
        assert!(
            message.starts_with(&format!("config error at {}:2:23: ", path.display())),
            "{message}"
        );
        assert!(
            message.contains("unknown field `auto_start_terminal`"),
            "{message}"
        );
        assert!(
            message.contains("auto_start_terminals"),
            "the known keys are listed, so the typo is obvious: {message}"
        );

        // Nested objects and project entries are held to the same rule.
        fs::write(&path, r##"{"colorscheme":{"foreground":"#ffffff"}}"##).expect("write config");
        assert!(load_from_path(&path)
            .expect_err("an unknown colorscheme key must be refused")
            .to_string()
            .contains("unknown field `foreground`"));

        fs::write(&path, r#"{"projects":[{"name":"a","pathh":"/tmp"}]}"#).expect("write config");
        assert!(load_from_path(&path)
            .expect_err("an unknown project key must be refused")
            .to_string()
            .contains("unknown field `pathh`"));
    }

    /// G13/E6: a colour that does not parse keeps its default — that part was
    /// always true — but it is now *reported* instead of vanishing.
    #[test]
    fn invalid_color_strings_fall_back_to_the_default_palette_and_report() {
        let path = unique_temp_file();
        fs::write(
            &path,
            r##"{"colorscheme":{"text":"blue","base":"#12345","_nc":"#000001"}}"##,
        )
        .expect("write config");

        let config = load_from_path(&path).expect("a bad colour is not a startup error");

        // The two unparsable keys keep the built-in colours; the valid one is
        // applied, so this is not simply "the whole scheme was discarded".
        let mut expected = Config::default();
        expected.colorscheme.nc = "#000001".to_string();
        assert_eq!(config.palette(), expected.palette());

        let warnings = config.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].contains(&format!("{}: colorscheme.base", path.display()))
                && warnings[0].contains("\"#12345\"")
                && warnings[0].contains("keeping the built-in default"),
            "{warnings:?}"
        );
        assert!(warnings[1].contains("colorscheme.text"), "{warnings:?}");
        assert!(
            Config::default().warnings().is_empty(),
            "a valid config warns about nothing"
        );
    }

    /// E6: a shortcut pointing nowhere is a warning, not a startup failure —
    /// the directory may simply not be mounted yet, and nothing has been asked
    /// of it.
    #[test]
    fn a_project_path_that_is_not_a_directory_is_a_warning_not_an_error() {
        let directory = unique_temp_dir();
        let present = directory.join("present");
        fs::create_dir(&present).expect("create project directory");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            format!(
                r#"{{"projects":[["here","{}"],["gone","{}"]]}}"#,
                present.display(),
                directory.join("absent").display()
            ),
        )
        .expect("write config");

        let config = load_from_path(&path).expect("a missing project path is not fatal");

        assert_eq!(config.projects.len(), 2, "the shortcut is still offered");
        let warnings = config.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("project \"gone\""), "{warnings:?}");
        assert!(warnings[0].contains("is not a directory"), "{warnings:?}");
    }

    /// C14: a symlinked `config.json` is the layout every dotfile manager
    /// produces — home-manager's `xdg.configFile` has no other mode — so it is
    /// resolved and read, not refused.
    #[test]
    fn a_symlinked_config_is_resolved_and_read() {
        let directory = unique_temp_dir();
        let target = directory.join("dotfiles-config.json");
        fs::write(&target, r#"{"pi_agent_command":"linked"}"#).expect("write target");
        let path = directory.join(CONFIG_FILE_NAME);
        symlink(&target, &path).expect("link config path");

        let config = load_from_path(&path).expect("a symlinked config is followed");

        assert_eq!(config.pi_agent_command, "linked");
    }

    /// C14, the case that actually bites: home-manager links the config
    /// *directory*, not the file, so the link is a component on the way rather
    /// than the leaf.
    #[test]
    fn a_symlinked_config_directory_is_resolved_and_read() {
        let directory = unique_temp_dir();
        let real = directory.join("dotfiles");
        fs::create_dir(&real).expect("create target directory");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).expect("restrict target");
        fs::write(
            real.join(CONFIG_FILE_NAME),
            r#"{"pi_agent_command":"linked-directory"}"#,
        )
        .expect("write target");
        let linked = directory.join("config");
        symlink(&real, &linked).expect("link config directory");

        let config = load_from_path(&linked.join(CONFIG_FILE_NAME))
            .expect("a symlinked config directory is followed");

        assert_eq!(config.pi_agent_command, "linked-directory");
    }

    /// A link pointing nowhere is indistinguishable from no config at all, and
    /// a missing config has always meant defaults rather than a startup error.
    #[test]
    fn a_dangling_config_symlink_falls_back_to_defaults() {
        let directory = unique_temp_dir();
        let path = directory.join(CONFIG_FILE_NAME);
        symlink(directory.join("never-created.json"), &path).expect("link config path");

        let config = load_from_path(&path).expect("a dangling link is not a startup error");

        assert_eq!(config.pi_agent_command, default_pi_agent_command());
    }

    /// E5: the shared reader's directory checks used to say "state" even when
    /// it was the config directory that failed.
    #[test]
    fn a_rejected_config_directory_says_config_not_state() {
        let directory = unique_temp_dir();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o777))
            .expect("widen directory");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, "{}").expect("write config");

        let error = load_from_path(&path).expect_err("a replaceable parent must be refused");

        let message = error.to_string();
        assert!(
            message.starts_with("config parent is writable"),
            "{message}"
        );
        assert!(!message.contains("state"), "{message}");
    }

    /// C14 moved the checks from the path the user typed to the path it
    /// resolves to, so this is the planted-symlink route to a shell-evaluated
    /// `pi_agent_command`: the link is private, but it aims at a directory
    /// anyone can write. Following the link must not launder the target.
    #[test]
    fn a_symlink_into_a_world_writable_directory_is_refused() {
        let directory = unique_temp_dir();
        let reachable = directory.join("reachable");
        fs::create_dir(&reachable).expect("create target directory");
        fs::set_permissions(&reachable, fs::Permissions::from_mode(0o777)).expect("widen target");
        let target = reachable.join("planted.json");
        fs::write(&target, r#"{"pi_agent_command":"attacker"}"#).expect("write target");
        let path = directory.join(CONFIG_FILE_NAME);
        symlink(&target, &path).expect("link config path");

        let error = load_from_path(&path).expect_err("a replaceable target must be refused");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let message = error.to_string();
        assert!(
            message.starts_with("config parent is writable"),
            "{message}"
        );
    }

    /// The path the user typed and the file whose mode has to be fixed are
    /// different files once a link is involved; naming only one sends them to
    /// the wrong place.
    #[test]
    fn a_rejected_symlinked_config_names_both_paths() {
        let directory = unique_temp_dir();
        let target = directory.join("dotfiles-config.json");
        fs::write(&target, "{}").expect("write target");
        // A second hard link is the one file property `chmod` cannot repair.
        fs::hard_link(&target, directory.join("second-link.json")).expect("hard link target");
        let path = directory.join(CONFIG_FILE_NAME);
        symlink(&target, &path).expect("link config path");

        let error = load_from_path(&path).expect_err("a multiply-linked config must be refused");

        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(
            message.contains(&target.display().to_string()),
            "the resolved file is the one to fix: {message}"
        );
        assert!(message.contains("multiple hard links"), "{message}");
    }

    #[test]
    fn config_in_a_world_writable_directory_is_refused() {
        let directory = unique_temp_dir();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o777))
            .expect("widen directory");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, r#"{"pi_agent_command":"attacker"}"#).expect("write config");

        let error = load_from_path(&path).expect_err("a replaceable parent must be refused");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn config_read_is_size_capped_and_normalizes_the_file_mode() {
        let path = unique_temp_file();
        fs::write(&path, r#"{"pi_agent_command":"pi"}"#).expect("write config");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("widen config");

        load_from_path(&path).expect("load config");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let oversized = format!(
            r#"{{"pi_agent_command":"{}"}}"#,
            "x".repeat(MAX_CONFIG_FILE_BYTES)
        );
        fs::write(&path, oversized).expect("write oversized config");

        let error = load_from_path(&path).expect_err("an oversized config must be refused");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds"));
    }

    /// A path inside a fresh owner-only directory.
    ///
    /// Config is no longer read out of a shared, world-writable directory: the
    /// hardened read rejects a parent anyone else can write, so tests must
    /// build the same private parent a real installation has.
    fn unique_temp_file() -> PathBuf {
        unique_temp_dir().join("config.json")
    }

    fn unique_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "mult-config-test-{}-{unique}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create private config directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("restrict config directory");
        directory
    }
}
