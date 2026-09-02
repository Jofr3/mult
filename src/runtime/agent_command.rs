//! Building the shell command line that backs a chat, and generating the
//! private runtime artifacts it points at.
//!
//! Both agent backends report status into the same per-chat journal `mult`
//! polls, but through different mechanisms: pi loads a bundled extension
//! (`-e`), Claude Code gets a generated hooks settings file (`--settings`).

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use mult_protocol::{peer::effective_uid, shell::quote_argument};

use crate::{config::Config, model::AgentKind};

use super::ensure_mult_runtime_dir;

const MULT_STATUS_EXTENSION_SOURCE: &str = include_str!("../../extensions/mult-status.ts");

pub(super) const MULT_CLAUDE_STATUS_SCRIPT_SOURCE: &str =
    include_str!("../../extensions/mult-claude-status.sh");

/// Build the shell command line that backs a chat, chosen by its agent kind.
/// Both backends report status into the same per-chat file that `mult` polls,
/// but through different mechanisms: pi loads a bundled extension (`-e`), while
/// Claude Code gets a generated hooks settings file (`--settings`).
pub(super) fn agent_command(config: &Config, agent: AgentKind) -> String {
    match agent {
        AgentKind::Pi => pi_command_with_mult_status_extension(config),
        AgentKind::ClaudeCode => claude_code_command_with_mult_status_hooks(config),
    }
}

/// The command a chat in a **remote** workspace runs, which is the configured
/// agent command and nothing else.
///
/// Neither backend's status plumbing can cross the connection: pi's `-e`
/// extension and Claude Code's `--settings` hooks are files written into
/// `mult`'s private runtime directory *here*, and the hooks report by writing
/// into a journal *here* too. Passing those paths to an agent over there would
/// at best do nothing and at worst stop the agent starting, because the file it
/// was told to load does not exist on that machine. So the agent runs plain,
/// and the chat's status dot stays idle — the pane still shows everything the
/// agent prints, which is what a remote chat is for.
pub(super) fn remote_agent_command(config: &Config, agent: AgentKind) -> String {
    match agent {
        AgentKind::Pi => pi_command(config),
        AgentKind::ClaudeCode => claude_code_command(config),
    }
}

fn pi_command(config: &Config) -> String {
    let command = config.pi_agent_command.trim();
    if command.is_empty() {
        "pi".to_string()
    } else {
        command.to_string()
    }
}

fn claude_code_command(config: &Config) -> String {
    let command = config.claude_code_command.trim();
    if command.is_empty() {
        "claude".to_string()
    } else {
        command.to_string()
    }
}

fn pi_command_with_mult_status_extension(config: &Config) -> String {
    let command = pi_command(config);
    let Some(extension) = write_mult_status_extension_file() else {
        return command;
    };

    format!(
        "{command} -e {}",
        quote_argument(&extension.display().to_string())
    )
}

/// Append `--settings <file>` pointing at a generated hooks file that reports
/// chat status into the file `mult` polls. `--settings` merges over the user's
/// own Claude Code settings for this session only, so it does not touch their
/// config on disk. If the files cannot be written, fall back to the plain
/// command — Claude Code still runs, just without a live status dot.
fn claude_code_command_with_mult_status_hooks(config: &Config) -> String {
    let command = claude_code_command(config);
    let Some(settings) = write_mult_claude_status_files() else {
        return command;
    };

    format!(
        "{command} --settings {}",
        quote_argument(&settings.display().to_string())
    )
}

fn write_mult_status_extension_file() -> Option<PathBuf> {
    let dir = ensure_mult_runtime_dir().ok()?;
    write_private_runtime_file(
        &dir,
        "mult-status-extension",
        "ts",
        MULT_STATUS_EXTENSION_SOURCE.as_bytes(),
    )
}

/// Write the bundled status-writer script and a Claude Code settings file whose
/// hooks invoke it, returning the settings path to hand to `--settings`. Two
/// files because the settings JSON must reference the script by absolute path.
fn write_mult_claude_status_files() -> Option<PathBuf> {
    let dir = ensure_mult_runtime_dir().ok()?;
    let script = write_private_runtime_file(
        &dir,
        "mult-claude-status",
        "sh",
        MULT_CLAUDE_STATUS_SCRIPT_SOURCE.as_bytes(),
    )?;
    let settings = mult_claude_status_settings_json(&script);
    write_private_runtime_file(&dir, "mult-claude-settings", "json", settings.as_bytes())
}

/// Build the Claude Code `--settings` JSON that maps lifecycle hook events to
/// `mult` statuses by invoking the bundled script with the status as its
/// argument. Built with `serde_json` so the script path is correctly escaped
/// into the embedded shell command.
fn mult_claude_status_settings_json(script: &Path) -> String {
    let script = quote_argument(&script.display().to_string());
    let hook = |status: &str| {
        serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": format!("sh {script} {status}") }],
        })
    };

    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [hook("idle")],
            "UserPromptSubmit": [hook("running")],
            "PreToolUse": [hook("running")],
            "Notification": [hook("waiting")],
            "Stop": [hook("finished")],
        },
    });

    serde_json::to_string(&settings).unwrap_or_default()
}

fn write_private_runtime_file(
    dir: &Path,
    prefix: &str,
    extension: &str,
    contents: &[u8],
) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    // Versioned fixed names bound generated artifacts to three files instead of
    // leaking a PID/random file on every command construction.
    let path = dir.join(format!("{prefix}-v2.{extension}"));
    rotate_legacy_runtime_artifacts(dir, prefix, &path).ok()?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(&path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid()
        || metadata.nlink() != 1
        || metadata.len() > 1024 * 1024
    {
        return None;
    }
    let _guard = RuntimeFileLock::try_acquire(&file)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .ok()?;
    let mut existing = Vec::new();
    file.read_to_end(&mut existing).ok()?;
    if existing != contents {
        file.set_len(0).ok()?;
        file.seek(SeekFrom::Start(0)).ok()?;
        file.write_all(contents).ok()?;
        file.sync_all().ok()?;
    }
    Some(path)
}

/// How many times a contended runtime artifact is retried, and how long each
/// wait is. Two `mult` instances only collide while one of them writes three
/// small files, so a bounded wait of a few milliseconds resolves virtually
/// every real contention — and never costs the render loop more than
/// [`RUNTIME_LOCK_ATTEMPTS`] × [`RUNTIME_LOCK_RETRY_DELAY`] (S5).
const RUNTIME_LOCK_ATTEMPTS: u32 = 5;

const RUNTIME_LOCK_RETRY_DELAY: Duration = Duration::from_millis(2);

/// An exclusive `flock` held for the length of a runtime-artifact write.
struct RuntimeFileLock(std::os::fd::RawFd);

impl RuntimeFileLock {
    /// Take the lock without ever blocking the render thread.
    ///
    /// A blocking `LOCK_EX` here meant a second `mult` starting an agent could
    /// stall the first one's whole event loop for as long as it took to write
    /// its files. `LOCK_NB` plus a bounded retry turns that into a short wait
    /// and, at worst, a caller that degrades (the agent starts without the
    /// status extension) instead of a frozen UI.
    fn try_acquire(file: &fs::File) -> Option<Self> {
        use std::os::fd::AsRawFd;

        let descriptor = file.as_raw_fd();
        for attempt in 0..RUNTIME_LOCK_ATTEMPTS {
            if unsafe { libc::flock(descriptor, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Some(Self(descriptor));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK)
                && error.raw_os_error() != Some(libc::EINTR)
            {
                return None;
            }
            if attempt + 1 < RUNTIME_LOCK_ATTEMPTS {
                std::thread::sleep(RUNTIME_LOCK_RETRY_DELAY);
            }
        }
        None
    }
}

impl Drop for RuntimeFileLock {
    fn drop(&mut self) {
        // The descriptor is closed right after this, which would release the
        // lock anyway; unlocking explicitly keeps the window closed even if the
        // file ever outlives the guard.
        unsafe { libc::flock(self.0, libc::LOCK_UN) };
    }
}

fn rotate_legacy_runtime_artifacts(
    directory: &Path,
    prefix: &str,
    current: &Path,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    const MAX_RETAINED_LEGACY_ARTIFACTS: usize = 16;
    let mut candidates = Vec::new();
    for entry in fs::read_dir(directory)?.take(4096) {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if path == current || !name.starts_with(prefix) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_file()
            && metadata.uid() == effective_uid()
            && metadata.nlink() == 1
        {
            candidates.push((metadata.modified().ok(), path));
        }
    }
    candidates.sort_by_key(|(modified, _)| *modified);
    let remove_count = candidates
        .len()
        .saturating_sub(MAX_RETAINED_LEGACY_ARTIFACTS);
    for (_, path) in candidates.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::runtime::test_support::*;

    /// S5: a second `mult` writing the same runtime artifact must not stall
    /// this one's render loop. The lock is taken non-blocking with a bounded
    /// retry, so contention degrades (no status extension) instead of hanging.
    #[test]
    fn a_contended_runtime_artifact_lock_gives_up_instead_of_blocking() {
        use std::os::unix::fs::OpenOptionsExt;

        let directory = unique_status_path("runtime-lock").with_extension("dir");
        fs::create_dir_all(&directory).expect("create runtime artifact directory");
        let path = directory.join("mult-status-extension-v2.ts");
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .expect("create contended artifact");
        assert_eq!(
            unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&holder),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            },
            0,
            "the test takes the lock the other instance would hold"
        );

        assert!(
            write_private_runtime_file(&directory, "mult-status-extension", "ts", b"source")
                .is_none(),
            "a contended write reports failure instead of blocking the render loop"
        );

        drop(holder);
        assert_eq!(
            write_private_runtime_file(&directory, "mult-status-extension", "ts", b"source")
                .as_deref(),
            Some(path.as_path()),
            "and succeeds once the other instance is done"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn pi_command_comes_from_config_with_default_fallback() {
        assert_eq!(
            pi_command(&config_with(|config| {
                config.pi_agent_command = "pi -c".to_string()
            })),
            "pi -c"
        );
        assert_eq!(
            pi_command(&config_with(|config| {
                config.pi_agent_command = "   ".to_string()
            })),
            "pi"
        );
    }

    #[test]
    fn claude_code_command_comes_from_config_with_default_fallback() {
        assert_eq!(
            claude_code_command(&config_with(|config| {
                config.claude_code_command = "claude --resume".to_string()
            })),
            "claude --resume"
        );
        assert_eq!(
            claude_code_command(&config_with(|config| {
                config.claude_code_command = "   ".to_string()
            })),
            "claude"
        );
    }

    #[test]
    fn pi_command_appends_mult_status_extension_when_available() {
        let command = pi_command_with_mult_status_extension(&config_with(|config| {
            config.pi_agent_command = "pi --model test".to_string()
        }));

        assert!(command.starts_with("pi --model test"));
        assert!(command.contains(" -e "));
        assert!(command.contains("mult-status-extension-"));
    }

    #[test]
    fn agent_command_routes_by_kind() {
        let config = config_with(|config| {
            config.pi_agent_command = "pi".to_string();
            config.claude_code_command = "claude --here".to_string();
        });

        // Pi takes the bundled status extension (`-e`); Claude Code takes a
        // generated hooks settings file (`--settings`). Neither borrows the
        // other's flag.
        let pi = agent_command(&config, AgentKind::Pi);
        assert!(pi.starts_with("pi"));
        assert!(pi.contains(" -e "));
        assert!(!pi.contains(" --settings "));

        let cc = agent_command(&config, AgentKind::ClaudeCode);
        assert!(cc.starts_with("claude --here"));
        assert!(cc.contains(" --settings "));
        assert!(!cc.contains(" -e "));
    }

    /// The status artifacts are local files, so a remote agent is launched
    /// without them instead of being handed paths that do not exist on the
    /// machine it runs on.
    #[test]
    fn a_remote_agent_command_carries_no_local_status_artifacts() {
        let config = config_with(|config| {
            config.pi_agent_command = "pi --model test".to_string();
            config.claude_code_command = "claude --resume".to_string();
        });

        let pi = remote_agent_command(&config, AgentKind::Pi);
        assert_eq!(pi, "pi --model test");
        assert!(!pi.contains(" -e "));

        let claude = remote_agent_command(&config, AgentKind::ClaudeCode);
        assert_eq!(claude, "claude --resume");
        assert!(!claude.contains(" --settings "));
    }

    #[test]
    fn claude_code_command_appends_mult_status_hooks_when_available() {
        let command = claude_code_command_with_mult_status_hooks(&config_with(|config| {
            config.claude_code_command = "claude --model test".to_string()
        }));

        assert!(command.starts_with("claude --model test"));
        assert!(command.contains(" --settings "));
        assert!(command.contains("mult-claude-settings-"));
    }

    #[test]
    fn mult_claude_status_settings_json_maps_each_event() {
        let json = mult_claude_status_settings_json(Path::new("/run/mult/status.sh"));
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid settings json");

        // Each lifecycle event registers one matcher-less command hook that runs
        // the bundled script with the mult status for that event.
        for (event, status) in [
            ("SessionStart", "idle"),
            ("UserPromptSubmit", "running"),
            ("PreToolUse", "running"),
            ("Notification", "waiting"),
            ("Stop", "finished"),
        ] {
            let entry = &value["hooks"][event][0];
            assert_eq!(entry["matcher"], "");
            let command = entry["hooks"][0]["command"]
                .as_str()
                .expect("command is a string");
            assert_eq!(entry["hooks"][0]["type"], "command");
            assert!(
                command.starts_with("sh /run/mult/status.sh "),
                "unexpected command for {event}: {command}"
            );
            assert!(
                command.ends_with(&format!(" {status}")),
                "event {event} should map to status {status}, got {command}"
            );
        }
    }
}
