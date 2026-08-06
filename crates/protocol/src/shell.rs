//! Shell conventions shared by the client and the daemon.
//!
//! Both ends decide what to `exec` and how to render a command line for the
//! user, and both used to carry their own copy of every rule here (F20). The
//! copies had already drifted: the two quoting helpers disagreed about which
//! characters are safe unquoted and about the empty string, which is why they
//! are still two functions rather than one.

/// The login shell to launch a session in.
///
/// `$SHELL` is the user's own choice; `/bin/sh` is the fallback that must
/// exist.
#[must_use]
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

/// Argv for running `command` through [`default_shell`].
///
/// The command string is handed to the login shell for evaluation (`-lc`), so
/// it is fully shell-interpreted: pipelines, `$VAR` expansion, and globbing all
/// apply. This is by design for `pi_agent_command`, `claude_code_command` and
/// `TerminalLaunch::Command`, and is the deliberate difference from
/// `MULT_AGENT_CMD`, which `mult` splits into argv with no shell. See
/// `AGENTS.md`.
#[must_use]
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

/// Wrap `value` in single quotes, escaping any it already contains.
///
/// The quoting core both helpers below share: `'` closes the quote, escapes a
/// literal quote, and reopens it.
#[must_use]
fn single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Quote `value` for a shell command line that will be **executed**.
///
/// The empty string becomes `''`, because an argument that vanished is not the
/// same argument.
#[must_use]
pub fn quote_argument(value: &str) -> String {
    if value.is_empty() {
        return single_quote(value);
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '+'))
    {
        return value.to_string();
    }

    single_quote(value)
}

/// Quote `value` for a command line only ever **shown** to the user, such as a
/// pane title.
///
/// Deliberately not [`quote_argument`]: `=` is left bare so `KEY=value` reads
/// plainly, `+` is not, and the empty string renders as nothing rather than
/// `''` because this string is never executed. Merging the two would have
/// changed both outputs, so they share `single_quote` and nothing else.
#[must_use]
pub fn quote_display_argument(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return value.to_string();
    }

    single_quote(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_command_is_evaluated_by_the_login_shell() {
        assert_eq!(
            shell_command_args("ls | wc -l".to_string()),
            vec!["-lc".to_string(), "ls | wc -l".to_string()]
        );
    }

    #[test]
    fn quoting_leaves_safe_arguments_bare_and_escapes_the_rest() {
        assert_eq!(quote_argument("/tmp/no-spaces.ts"), "/tmp/no-spaces.ts");
        assert_eq!(quote_argument("/tmp/has space.ts"), "'/tmp/has space.ts'");
        assert_eq!(quote_argument("/tmp/it's.ts"), "'/tmp/it'\\''s.ts'");
    }

    /// F20: the two helpers were near-duplicates, and merging them would have
    /// changed output. These are the three inputs they disagree on.
    #[test]
    fn execution_and_display_quoting_differ_where_they_always_did() {
        assert_eq!(quote_argument(""), "''");
        assert_eq!(quote_display_argument(""), "");

        assert_eq!(quote_argument("a+b"), "a+b");
        assert_eq!(quote_display_argument("a+b"), "'a+b'");

        assert_eq!(quote_argument("KEY=value"), "'KEY=value'");
        assert_eq!(quote_display_argument("KEY=value"), "KEY=value");
    }
}
