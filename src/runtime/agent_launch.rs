//! Building the shell command line that backs a chat, and writing the private
//! runtime files those command lines point at.
//!
//! Both backends report status into the same per-chat file that `mult` polls
//! (see [`super::agent_status`]), but through different mechanisms: pi loads a
//! bundled extension (`-e`), while Claude Code gets a generated hooks settings
//! file (`--settings`). Everything generated here is written 0600 into a
//! directory that is ownership- and mode-checked first.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use mult_protocol::{rand::random_u64, shell::quote as shell_quote};

use crate::{config::Config, model::AgentKind};

/// The pi status-reporting extension, compiled in so a `mult` binary always has
/// the exact extension its own status parser expects.
pub(super) const MULT_STATUS_EXTENSION_SOURCE: &str =
    include_str!("../../extensions/mult-status.ts");
/// The Claude Code status-reporting hook script, compiled in for the same reason.
pub(super) const MULT_CLAUDE_STATUS_SCRIPT_SOURCE: &str =
    include_str!("../../extensions/mult-claude-status.sh");

/// Build the shell command line that backs a chat, chosen by its agent kind.
/// Both backends report status into the same per-chat file that `mult` polls,
/// but through different mechanisms: pi loads a bundled extension (`-e`), while
/// Claude Code gets a generated hooks settings file (`--settings`).
pub(super) fn agent_command(config: &Config, agent: AgentKind) -> String {
    agent_command_in(ensure_mult_runtime_dir().ok().as_deref(), config, agent)
}

/// Testable core of [`agent_command`]: the directory the generated status files
/// go in arrives as an argument rather than being resolved from the environment
/// (G15).
///
/// `None` means there is nowhere private to write them, which is not an error —
/// the agent still runs, just without a live status dot.
fn agent_command_in(dir: Option<&Path>, config: &Config, agent: AgentKind) -> String {
    match agent {
        AgentKind::Pi => pi_command_with_mult_status_extension(dir, config),
        AgentKind::ClaudeCode => claude_code_command_with_mult_status_hooks(dir, config),
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

fn pi_command_with_mult_status_extension(dir: Option<&Path>, config: &Config) -> String {
    let command = pi_command(config);
    let Some(extension) = dir.and_then(write_mult_status_extension_file) else {
        return command;
    };

    format!(
        "{command} -e {}",
        shell_quote(&extension.display().to_string())
    )
}

/// Append `--settings <file>` pointing at a generated hooks file that reports
/// chat status into the file `mult` polls. `--settings` merges over the user's
/// own Claude Code settings for this session only, so it does not touch their
/// config on disk. If the files cannot be written, fall back to the plain
/// command — Claude Code still runs, just without a live status dot.
fn claude_code_command_with_mult_status_hooks(dir: Option<&Path>, config: &Config) -> String {
    let command = claude_code_command(config);
    let Some(settings) = dir.and_then(write_mult_claude_status_files) else {
        return command;
    };

    format!(
        "{command} --settings {}",
        shell_quote(&settings.display().to_string())
    )
}

fn write_mult_status_extension_file(dir: &Path) -> Option<PathBuf> {
    write_private_runtime_file(
        dir,
        "mult-status-extension",
        "ts",
        MULT_STATUS_EXTENSION_SOURCE.as_bytes(),
    )
}

/// Write the bundled status-writer script and a Claude Code settings file whose
/// hooks invoke it, returning the settings path to hand to `--settings`. Two
/// files because the settings JSON must reference the script by absolute path.
fn write_mult_claude_status_files(dir: &Path) -> Option<PathBuf> {
    let script = write_private_runtime_file(
        dir,
        "mult-claude-status",
        "sh",
        MULT_CLAUDE_STATUS_SCRIPT_SOURCE.as_bytes(),
    )?;
    let settings = mult_claude_status_settings_json(&script);
    write_private_runtime_file(dir, "mult-claude-settings", "json", settings.as_bytes())
}

/// Build the Claude Code `--settings` JSON that maps lifecycle hook events to
/// `mult` statuses by invoking the bundled script with the status as its
/// argument. Built with `serde_json` so the script path is correctly escaped
/// into the embedded shell command.
fn mult_claude_status_settings_json(script: &Path) -> String {
    let script = shell_quote(&script.display().to_string());
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

/// Write one of the generated runtime files (the pi extension, the Claude Code
/// hook script, its settings JSON) and return its path.
///
/// The name is derived from the *contents*, not from the pid and a random
/// suffix. That matters because these files are written on every agent start
/// and were never removed: the runtime directory accumulated one more copy of
/// an executable hook script per chat per session, indefinitely, and Claude
/// Code is configured to run whatever the settings file points at. Naming by
/// content means a given `mult` build writes exactly one of each, and every
/// later start reuses it — the directory stops growing without needing a
/// shutdown hook that a crash would skip.
///
/// An existing file is reused only when it is genuinely ours and byte-identical
/// (`read_private_runtime_file` re-checks owner, type and mode); otherwise it is
/// replaced atomically, so a reader never sees a half-written script.
fn write_private_runtime_file(
    dir: &Path,
    prefix: &str,
    extension: &str,
    contents: &[u8],
) -> Option<PathBuf> {
    let path = dir.join(format!(
        "{prefix}-{:016x}.{extension}",
        content_digest(contents)
    ));
    if read_private_runtime_file(&path, contents.len()).as_deref() == Some(contents) {
        return Some(path);
    }

    replace_private_file(&path, contents).ok()?;
    Some(path)
}

fn read_private_runtime_file(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    mult_protocol::read_private_file(path, max_bytes as u64).ok()
}

/// FNV-1a. Not a security property — the file's *contents* are checked before
/// it is reused, so a collision costs a rewrite, not a wrong script.
fn content_digest(contents: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in contents {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// Create `path` with mode 0600 and exactly `contents`, replacing whatever was
/// there. The write goes to a fresh temp file in the same directory and is
/// renamed into place, so the path is never observed partially written.
fn replace_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file_name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    file_name.push(format!(
        ".tmp-{}-{:016x}",
        std::process::id(),
        random_u64()?
    ));
    let temp_path = path.with_file_name(file_name);

    let result = (|| {
        write_private_file(&temp_path, contents)?;
        fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

pub(super) fn ensure_mult_runtime_dir() -> io::Result<PathBuf> {
    ensure_private_runtime_dir(mult_runtime_dir())
}

/// Pure core of [`ensure_mult_runtime_dir`]: takes the candidate directory so
/// the "a rejected directory is never returned anyway" rule is testable without
/// touching `$XDG_RUNTIME_DIR`.
fn ensure_private_runtime_dir(dir: PathBuf) -> io::Result<PathBuf> {
    mult_protocol::ensure_private_dir(&dir)?;
    Ok(dir)
}

/// A private directory of a test's own, for tests about generated file
/// *contents* (G15).
///
/// Deliberately not [`mult_protocol::ensure_private_dir`]: that walks every
/// ancestor and refuses one owned by another user, and inside a Nix build
/// sandbox no path satisfies it — `/` there is owned by neither the build user
/// nor root, so the walk rejects `$TMPDIR` and everything under it. Six tests
/// that are about what `mult` writes, not about whose directory it writes into,
/// failed for that reason alone and made `nix flake check` red.
///
/// The check itself still has its own tests (`ensure_private_runtime_dir` is
/// called directly by `a_rejected_runtime_dir_is_never_used_for_status_files`),
/// so nothing here weakens what is covered — it just stops unrelated tests from
/// depending on the ambient filesystem. The directory is 0700 and ours by
/// construction, which is what the code under test needs.
#[cfg(test)]
pub(super) fn private_test_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let dir =
        std::env::temp_dir().join(format!("mult-{label}-test-{}-{nanos}", std::process::id()));

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir).expect("create private test dir");
    dir
}

pub(super) fn mult_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("mult-{}", mult_protocol::peer::effective_uid()))
        })
        .join("mult")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_command_comes_from_config_with_default_fallback() {
        assert_eq!(
            pi_command(&Config {
                pi_agent_command: "pi -c".to_string(),
                ..Config::default()
            }),
            "pi -c"
        );
        assert_eq!(
            pi_command(&Config {
                pi_agent_command: "   ".to_string(),
                ..Config::default()
            }),
            "pi"
        );
    }
    #[test]
    fn claude_code_command_comes_from_config_with_default_fallback() {
        assert_eq!(
            claude_code_command(&Config {
                claude_code_command: "claude --resume".to_string(),
                ..Config::default()
            }),
            "claude --resume"
        );
        assert_eq!(
            claude_code_command(&Config {
                claude_code_command: "   ".to_string(),
                ..Config::default()
            }),
            "claude"
        );
    }
    #[test]
    fn pi_command_appends_mult_status_extension_when_available() {
        let dir = private_test_dir("pi-extension");
        let command = pi_command_with_mult_status_extension(
            Some(&dir),
            &Config {
                pi_agent_command: "pi --model test".to_string(),
                ..Config::default()
            },
        );

        assert!(command.starts_with("pi --model test"));
        assert!(command.contains(" -e "));
        assert!(command.contains("mult-status-extension-"));

        let _ = fs::remove_dir_all(&dir);
    }
    /// With nowhere private to write the extension, the agent still runs — it
    /// just loses the status dot, which is what the `Option` is for.
    #[test]
    fn commands_lose_only_their_status_reporting_without_a_runtime_dir() {
        let config = Config {
            pi_agent_command: "pi".to_string(),
            claude_code_command: "claude".to_string(),
            ..Config::default()
        };

        assert_eq!(agent_command_in(None, &config, AgentKind::Pi), "pi");
        assert_eq!(
            agent_command_in(None, &config, AgentKind::ClaudeCode),
            "claude"
        );
    }
    #[test]
    fn agent_command_routes_by_kind() {
        let dir = private_test_dir("agent-command");
        let config = Config {
            pi_agent_command: "pi".to_string(),
            claude_code_command: "claude --here".to_string(),
            ..Config::default()
        };

        // Pi takes the bundled status extension (`-e`); Claude Code takes a
        // generated hooks settings file (`--settings`). Neither borrows the
        // other's flag.
        let pi = agent_command_in(Some(&dir), &config, AgentKind::Pi);
        assert!(pi.starts_with("pi"));
        assert!(pi.contains(" -e "));
        assert!(!pi.contains(" --settings "));

        let cc = agent_command_in(Some(&dir), &config, AgentKind::ClaudeCode);
        assert!(cc.starts_with("claude --here"));
        assert!(cc.contains(" --settings "));
        assert!(!cc.contains(" -e "));

        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn claude_code_command_appends_mult_status_hooks_when_available() {
        let dir = private_test_dir("claude-hooks");
        let command = claude_code_command_with_mult_status_hooks(
            Some(&dir),
            &Config {
                claude_code_command: "claude --model test".to_string(),
                ..Config::default()
            },
        );

        assert!(command.starts_with("claude --model test"));
        assert!(command.contains(" --settings "));
        assert!(command.contains("mult-claude-settings-"));

        let _ = fs::remove_dir_all(&dir);
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
    #[test]
    fn shell_quote_handles_paths_with_spaces() {
        assert_eq!(shell_quote("/tmp/no-spaces.ts"), "/tmp/no-spaces.ts");
        assert_eq!(shell_quote("/tmp/has space.ts"), "'/tmp/has space.ts'");
        assert_eq!(shell_quote("/tmp/it's.ts"), "'/tmp/it'\\''s.ts'");
    }
    #[cfg(unix)]
    #[test]
    fn a_rejected_runtime_dir_is_never_used_for_status_files() {
        use std::os::unix::fs::PermissionsExt;

        // Root ignores the mode bits, so the rejection would not happen and the
        // test would assert nothing.
        if mult_protocol::peer::effective_uid() == 0 {
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "mult-runtime-dir-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).expect("chmod");

        // The old code did `.unwrap_or_else(|_| mult_runtime_dir())`, handing
        // back the very directory the privacy check had just rejected. Failing
        // is the whole point: a status path is not worth an untrusted directory.
        let error = ensure_private_runtime_dir(dir.clone())
            .expect_err("a group/other-writable runtime dir must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let _ = fs::remove_dir_all(&dir);
    }
    #[cfg(unix)]
    #[test]
    fn generated_runtime_files_are_reused_rather_than_accumulated() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_test_dir("runtime-files");

        let first = write_private_runtime_file(&dir, "hook", "sh", b"echo one")
            .expect("write generated file");
        let again = write_private_runtime_file(&dir, "hook", "sh", b"echo one")
            .expect("rewrite generated file");
        let other = write_private_runtime_file(&dir, "hook", "sh", b"echo two")
            .expect("write different contents");

        // Same contents, same path: an agent start no longer leaves another
        // executable script behind, which is what used to grow without bound.
        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(fs::read(&first).expect("read generated file"), b"echo one");
        assert_eq!(
            fs::metadata(&first)
                .expect("generated file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let entries = fs::read_dir(&dir)
            .expect("read runtime dir")
            .filter_map(Result::ok)
            .count();
        assert_eq!(
            entries, 2,
            "one file per distinct content, no temp leftovers"
        );

        let _ = fs::remove_dir_all(&dir);
    }
    #[cfg(unix)]
    #[test]
    fn the_claude_status_hook_writes_through_a_private_unpredictable_temp_file() {
        use std::os::unix::fs::PermissionsExt;

        // The old script used `${path}.$$.tmp` with a plain `>` redirect: a
        // predictable name, created without O_EXCL and following symlinks, so a
        // pre-planted link had this hook truncate and overwrite its target.
        assert!(
            !MULT_CLAUDE_STATUS_SCRIPT_SOURCE.contains(".$$.tmp"),
            "the temp file name must not be derived from the pid"
        );
        assert!(MULT_CLAUDE_STATUS_SCRIPT_SOURCE.contains("mktemp"));

        let dir = private_test_dir("claude-hook");
        let script = dir.join("hook.sh");
        fs::write(&script, MULT_CLAUDE_STATUS_SCRIPT_SOURCE).expect("write hook script");
        let status_path = dir.join("status.json");

        let output = std::process::Command::new("sh")
            .arg(&script)
            .arg("running")
            .env("MULT_AGENT_STATUS_PATH", &status_path)
            .env("MULT_AGENT_CHAT_ID", "7")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run hook script");
        assert!(output.status.success());

        let written = fs::read_to_string(&status_path).expect("hook wrote the status file");
        assert!(written.contains(r#""status":"running""#), "{written}");
        assert!(written.contains(r#""chatId":"7""#), "{written}");
        // mktemp creates 0600 and the rename preserves it.
        assert_eq!(
            fs::metadata(&status_path)
                .expect("status metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let leftovers = fs::read_dir(&dir)
            .expect("read hook dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "the temp file must not be left behind");

        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn the_pi_status_extension_creates_its_temp_file_exclusively() {
        // Same defect as the shell hook, in the TypeScript writer. The
        // behaviour itself is covered by `npm run typecheck` plus review; what
        // is pinned here is that the embedded source cannot silently regress to
        // the predictable, symlink-following write.
        assert!(
            !MULT_STATUS_EXTENSION_SOURCE.contains("${process.pid}.tmp"),
            "the temp file name must not be derived from the pid"
        );
        assert!(MULT_STATUS_EXTENSION_SOURCE.contains("O_EXCL"));
        assert!(MULT_STATUS_EXTENSION_SOURCE.contains("O_NOFOLLOW"));
        assert!(MULT_STATUS_EXTENSION_SOURCE.contains("mode: 0o700"));
        assert!(MULT_STATUS_EXTENSION_SOURCE.contains("0o600"));
    }
}
