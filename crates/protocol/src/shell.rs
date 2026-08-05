//! Login-shell invocation and argument quoting, shared by the client and the
//! daemon.
//!
//! Both sides build the same `$SHELL -lc <command>` invocation — the client to
//! describe a session it is asking for, the daemon to actually spawn it — so the
//! two must agree byte for byte or a session's `LaunchSpec` would not mean the
//! same thing at each end. They used to be two copies (F20).

/// The login shell a PTY is started with.
///
/// `$SHELL` is the user's own choice and is used verbatim; `/bin/sh` is the
/// fallback for an environment that does not set it (a cron-like parent, a
/// stripped daemon environment).
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
    }

    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

/// The argv that hands `command` to [`default_shell`] for evaluation.
///
/// The command string is fully shell-interpreted (`-lc`): pipelines, `$VAR`
/// expansion, and globbing all apply. That is by design for `pi_agent_command`,
/// `claude_code_command` and `TerminalLaunch::Command`: the command line is the
/// user's own config, not a privilege boundary. See `AGENTS.md`.
pub fn shell_command_args(command: String) -> Vec<String> {
    #[cfg(windows)]
    {
        vec!["-NoExit".to_string(), "-Command".to_string(), command]
    }

    #[cfg(not(windows))]
    {
        vec!["-lc".to_string(), command]
    }
}

/// Characters a *generated* command line leaves unquoted.
///
/// Deliberately conservative: what this quotes is fed back to `$SHELL -lc`, so
/// anything outside the set is wrapped rather than risked. `=` is absent because
/// a bare `name=value` word is an assignment in command position.
const QUOTE_SAFE_CHARS: &[char] = &['/', '.', '_', '-', ':', '+'];

/// Characters a *displayed* command line leaves unquoted.
///
/// This set exists to make `/proc/<pid>/cmdline` readable in a sidebar label,
/// not to be re-executed, so `--flag=value` stays as the user typed it while `+`
/// is quoted. The two sets are genuinely different — see [`quote`] — and are
/// kept apart on purpose; only the quoting mechanism below is shared.
const DISPLAY_SAFE_CHARS: &[char] = &['-', '_', '.', '/', ':', '='];

/// Quote `value` for inclusion in a command line that will be evaluated by the
/// shell.
pub fn quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    quote_unless_all(value, QUOTE_SAFE_CHARS)
}

/// Quote one argument of a command line that is only ever *shown* to the user.
pub fn display_arg(arg: String) -> String {
    if arg.chars().all(|ch| is_safe(ch, DISPLAY_SAFE_CHARS)) {
        arg
    } else {
        quote_single(&arg)
    }
}

fn quote_unless_all(value: &str, safe: &[char]) -> String {
    if value.chars().all(|ch| is_safe(ch, safe)) {
        value.to_string()
    } else {
        quote_single(value)
    }
}

fn is_safe(ch: char, safe: &[char]) -> bool {
    ch.is_ascii_alphanumeric() || safe.contains(&ch)
}

/// Wrap in single quotes, ending and reopening the quoting around any embedded
/// quote (`'\''`) — the only form that is safe for arbitrary bytes.
fn quote_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_wraps_anything_the_shell_would_reinterpret() {
        assert_eq!(quote("/tmp/no-spaces.ts"), "/tmp/no-spaces.ts");
        assert_eq!(quote("/tmp/has space.ts"), "'/tmp/has space.ts'");
        assert_eq!(quote("/tmp/it's.ts"), "'/tmp/it'\\''s.ts'");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn display_quoting_keeps_flag_values_readable() {
        assert_eq!(display_arg("--model=local".to_string()), "--model=local");
        assert_eq!(display_arg("two words".to_string()), "'two words'");
    }

    #[test]
    fn shell_command_args_go_through_a_login_shell() {
        assert_eq!(
            shell_command_args("echo hi".to_string()).last().unwrap(),
            "echo hi"
        );
    }
}
