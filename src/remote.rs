//! Command lines for a workspace that lives on another machine.
//!
//! A remote workspace has no local directory: every pane in it is an `ssh`
//! into the configured destination, so what would have been `cwd` for a local
//! pane is a `cd` inside the remote command instead.
//!
//! Terminals are plain `ssh` — a shell, or a command, in the project
//! directory, and when the connection ends so do they, which is what a terminal
//! is. An **agent chat** is not: an agent is a long conversation that must
//! survive `mult` quitting, the laptop sleeping and the link dropping, so it is
//! started inside a `tmux` session on the remote machine, created on first use
//! and re-attached to every time after.
//!
//! Two shells parse what is built here, and they are not the same shell.
//! `mult` runs the whole line through the *local* login shell (`$SHELL -lc`,
//! the same as any other command terminal), and `ssh` hands its trailing
//! argument to the *remote* login shell. Everything below is therefore quoted
//! twice on purpose: [`quote_for_local_shell`] wraps what the remote shell must
//! receive verbatim, and the tokens inside it are quoted for the remote shell.
//! Neither layer assumes `bash` — the forms used here (`'…'`, `"…"`, `&&`,
//! `$HOME`) mean the same thing in every shell a login shell is likely to be,
//! including `fish`, which is why `${VAR:-default}` is deliberately not used.

use std::fmt;

use crate::model::RemoteTarget;

/// The `ssh` destination is malformed, so no command can be built from it.
///
/// Checked rather than assumed because the destination is the first argument
/// after `ssh`'s options: a value starting with `-` would be read as one, and a
/// value with whitespace in it would split into several. Both are config typos
/// with confusing failures, not attacks — but the same check is what keeps a
/// hand-edited state file from turning a workspace into extra `ssh` flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteError {
    EmptyDestination,
    DestinationLooksLikeAnOption(String),
    DestinationHasWhitespace(String),
    DestinationHasControlCharacter(String),
    EmptyPath,
    EmptyCommand,
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDestination => formatter.write_str("the remote destination is empty"),
            Self::DestinationLooksLikeAnOption(value) => write!(
                formatter,
                "the remote destination {value:?} starts with `-`, which ssh would read as an option"
            ),
            Self::DestinationHasWhitespace(value) => write!(
                formatter,
                "the remote destination {value:?} contains whitespace; write it as `user@host`"
            ),
            Self::DestinationHasControlCharacter(value) => write!(
                formatter,
                "the remote destination {value:?} contains a control character"
            ),
            Self::EmptyPath => formatter.write_str("the remote path is empty"),
            Self::EmptyCommand => formatter.write_str("the remote command is empty"),
        }
    }
}

impl std::error::Error for RemoteError {}

/// Checks an `ssh` destination as written in the config, returning it trimmed.
pub fn check_destination(destination: &str) -> Result<&str, RemoteError> {
    let destination = destination.trim();
    if destination.is_empty() {
        return Err(RemoteError::EmptyDestination);
    }
    if destination.starts_with('-') {
        return Err(RemoteError::DestinationLooksLikeAnOption(
            destination.to_string(),
        ));
    }
    if destination.chars().any(char::is_whitespace) {
        return Err(RemoteError::DestinationHasWhitespace(
            destination.to_string(),
        ));
    }
    if destination.chars().any(char::is_control) {
        return Err(RemoteError::DestinationHasControlCharacter(
            destination.to_string(),
        ));
    }
    Ok(destination)
}

/// The base `tmux` session name for a project called `name`.
///
/// `tmux` refuses `.` and `:` in a session name — they are its window and pane
/// separators — so a project named `docs.site` cannot be one verbatim. Rather
/// than fail on a name the user is entitled to, every character outside
/// `[A-Za-z0-9_-]` becomes `-`, which keeps the name recognisable in `tmux ls`
/// on the remote machine. The result is stored with the workspace, so renaming
/// the project later cannot move the session out from under the chats attached
/// to it.
pub fn session_name(name: &str) -> String {
    let sanitized = name
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "mult".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `ssh` into the target and create-or-attach the project's `tmux` session,
/// starting `command` in it the first time.
///
/// The session is named after the project and there is exactly one, which is
/// why a remote workspace holds exactly one agent chat: `tmux` mirrors a
/// session across every client attached to it, so a second chat on the same
/// name would be the same agent shown twice, with its own command silently
/// dropped by `new-session -A`.
///
/// Four `tmux` commands, not one, and the order is the point:
///
/// 1. `new-session -A -d` *ensures* the session exists without attaching —
///    creating it with the agent inside on the first run, and finding it
///    mid-conversation on every run after.
/// 2. `set-titles on` makes `tmux` forward a title to the terminal on the other
///    end of the `ssh`, which is this pane. Without it an agent's own title
///    stops at `tmux` and the sidebar row can only say "agent".
/// 3. `set-titles-string` asks for the pane's title *unless* it is still
///    `tmux`'s default, which is the remote machine's hostname — a row reading
///    `build-box` says nothing, and an empty title lets the chat's own name
///    stand until the agent has something to report.
/// 4. `attach-session` is what actually puts it on screen.
///
/// Both options are set with `-t` on the session: a remote machine's global
/// `tmux` configuration belongs to whoever set it up, and `mult` attaching to a
/// session is no reason to rewrite it.
pub fn tmux_agent_command(target: &RemoteTarget, command: &str) -> Result<String, RemoteError> {
    let destination = check_destination(&target.host)?;
    let directory = remote_path_token(&target.path)?;
    let session = quote_for_remote_shell(&target.session);
    let command = command.trim();
    if command.is_empty() {
        return Err(RemoteError::EmptyCommand);
    }
    let command = quote_for_remote_shell(command);
    let title = quote_for_remote_shell(AGENT_TITLE_FORMAT);
    // `;` reaches tmux as an argument of its own only if the remote shell is
    // stopped from reading it as a command separator, so it is quoted too.
    let remote = [
        format!("tmux new-session -A -d -s {session} -c {directory} {command}"),
        format!("';' set-option -t {session} set-titles on"),
        format!("';' set-option -t {session} set-titles-string {title}"),
        format!("';' attach-session -t {session}"),
    ]
    .join(" ");
    Ok(ssh_command(destination, &remote))
}

/// What `tmux` reports as the pane's title: the title itself, or nothing at all
/// while it is still the default `#{host}` every new pane starts with.
const AGENT_TITLE_FORMAT: &str = "#{?#{==:#T,#{host}},,#T}";

/// `ssh` into the target and start an interactive login shell in the project
/// directory — what a terminal in a remote workspace is.
///
/// No `tmux`: a terminal is the connection, and a shell that outlives the pane
/// showing it is a surprise, not a feature. What must survive a dropped link is
/// the agent, and [`tmux_agent_command`] is where that happens.
pub fn login_shell_command(target: &RemoteTarget) -> Result<String, RemoteError> {
    let destination = check_destination(&target.host)?;
    let directory = remote_path_token(&target.path)?;
    Ok(ssh_command(
        destination,
        &format!("cd {directory} && exec \"$SHELL\" -l"),
    ))
}

/// `ssh` into the target and run `command` in the project directory.
///
/// `command` is the user's own string and stays shell-evaluated, exactly as it
/// is for a local command terminal — the only difference is which machine's
/// login shell evaluates it. It is passed through untouched (the quoting around
/// it protects it from the *local* shell), so pipelines, `$VAR` and globs mean
/// what they mean on the remote side.
pub fn wrapped_command(target: &RemoteTarget, command: &str) -> Result<String, RemoteError> {
    let destination = check_destination(&target.host)?;
    let directory = remote_path_token(&target.path)?;
    Ok(ssh_command(
        destination,
        &format!("cd {directory} && {}", command.trim()),
    ))
}

/// `-t` because everything `mult` runs in a pane is interactive: without a
/// remote terminal, `tmux` refuses to start and full-screen programs render
/// into nothing.
fn ssh_command(destination: &str, remote: &str) -> String {
    format!(
        "ssh -t {} {}",
        quote_for_local_shell(destination),
        quote_for_local_shell(remote)
    )
}

/// The project's `.git/HEAD`, as a token for the remote shell.
///
/// The branch probe reads that one file rather than asking `git` — see
/// [`crate::runtime`]'s remote-branch probe and the refusal `crate::git`
/// documents.
pub fn head_file_token(path: &str) -> Result<String, RemoteError> {
    let path = path.trim().trim_end_matches('/');
    if path.is_empty() {
        return Err(RemoteError::EmptyPath);
    }
    remote_path_token(&format!("{path}/.git/HEAD"))
}

/// The project directory as the *remote* shell should read it.
///
/// A leading `~` is the remote user's home, not the local one, so it must not
/// be expanded here — but it also cannot be quoted, since quoting is what stops
/// a shell from expanding it. `$HOME` inside double quotes is the form that
/// crosses: it survives the local single quotes untouched, expands on the far
/// side, and still tolerates a space in the rest of the path.
fn remote_path_token(path: &str) -> Result<String, RemoteError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(RemoteError::EmptyPath);
    }
    if path == "~" {
        return Ok("\"$HOME\"".to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(format!("\"$HOME/{}\"", escape_for_double_quotes(rest)));
    }
    Ok(quote_for_remote_shell(path))
}

/// Single quotes, with an embedded `'` closed, escaped and reopened. The one
/// form that is literal in every shell, including `fish`.
fn quote_for_local_shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The same rule for the far side. The two are separate functions because they
/// answer to different shells and only happen to agree today; collapsing them
/// would hide which layer a call is quoting for.
fn quote_for_remote_shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Escapes what a double-quoted string still interprets, so only the `$HOME`
/// put there on purpose expands.
fn escape_for_double_quotes(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '"' | '\\' | '$' | '`') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(host: &str, path: &str, session: &str) -> RemoteTarget {
        RemoteTarget {
            host: host.to_string(),
            path: path.to_string(),
            session: session.to_string(),
        }
    }

    #[test]
    fn a_tilde_path_is_expanded_by_the_remote_shell_not_the_local_one() {
        let command =
            wrapped_command(&target("user@host", "~/projects/mult", "mult"), "ls").unwrap();

        assert_eq!(
            command,
            r#"ssh -t 'user@host' 'cd "$HOME/projects/mult" && ls'"#
        );
        // The local shell sees `$HOME` inside single quotes, so it cannot
        // substitute the local home before ssh ever runs.
        assert!(command.contains("'cd \"$HOME"));
    }

    #[test]
    fn an_absolute_path_is_quoted_for_the_remote_shell() {
        let command = wrapped_command(&target("host", "/srv/work space", "work"), "ls").unwrap();

        assert_eq!(
            command,
            r#"ssh -t 'host' 'cd '\''/srv/work space'\'' && ls'"#
        );
    }

    #[test]
    fn a_quote_in_a_path_survives_both_shells() {
        let command = login_shell_command(&target("host", "/srv/it's", "it-s")).unwrap();

        // Local: '…'\''…' rebuilds the single quote for ssh's argument, which
        // then hands the remote shell '/srv/it'\''s' — the path again.
        assert_eq!(
            command,
            r#"ssh -t 'host' 'cd '\''/srv/it'\''\'\'''\''s'\'' && exec "$SHELL" -l'"#
        );
    }

    #[test]
    fn a_command_terminal_runs_in_the_project_directory() {
        let command = wrapped_command(
            &target("user@host", "~/projects/mult", "mult"),
            "cargo test",
        )
        .unwrap();

        assert_eq!(
            command,
            r#"ssh -t 'user@host' 'cd "$HOME/projects/mult" && cargo test'"#
        );
    }

    #[test]
    fn a_dollar_sign_in_the_path_tail_does_not_expand_remotely() {
        let command = wrapped_command(&target("host", "~/$weird`dir", "weird"), "ls").unwrap();

        assert_eq!(command, r#"ssh -t 'host' 'cd "$HOME/\$weird\`dir" && ls'"#);
    }

    #[test]
    fn a_destination_that_ssh_would_read_as_an_option_is_refused() {
        assert_eq!(
            check_destination("-oProxyCommand=id"),
            Err(RemoteError::DestinationLooksLikeAnOption(
                "-oProxyCommand=id".to_string()
            ))
        );
        assert_eq!(check_destination("  "), Err(RemoteError::EmptyDestination));
        assert_eq!(
            check_destination("user@host extra"),
            Err(RemoteError::DestinationHasWhitespace(
                "user@host extra".to_string()
            ))
        );
        assert_eq!(check_destination(" user@host "), Ok("user@host"));
    }

    #[test]
    fn an_empty_path_is_refused_rather_than_running_in_the_remote_home() {
        assert_eq!(
            tmux_agent_command(&target("host", "   ", "mult"), "pi"),
            Err(RemoteError::EmptyPath)
        );
        assert_eq!(
            tmux_agent_command(&target("host", "/srv/x", "mult"), "   "),
            Err(RemoteError::EmptyCommand)
        );
    }

    /// The quoting claims are about two real shells, so a real shell checks
    /// them: `printf` in place of `ssh` prints the argument vector the local
    /// shell built, and `printf` in place of `tmux` prints what the remote one
    /// would have been handed. A path with a space, a single quote and a `~`
    /// exercises all three rules at once, and the result is the whole `tmux`
    /// invocation as `tmux` itself would see it.
    #[test]
    fn both_shells_split_the_command_into_the_arguments_it_was_built_from() {
        let command = tmux_agent_command(
            &target("user@host", "~/pro jects/it's", "mult"),
            "claude --resume",
        )
        .unwrap();

        let local_arguments = command
            .strip_prefix("ssh -t ")
            .expect("the command runs ssh");
        let local = sh_arguments(local_arguments, None);
        assert_eq!(
            local.len(),
            2,
            "ssh must see one destination and one command"
        );
        assert_eq!(local[0], "user@host");

        let remote_arguments = local[1]
            .strip_prefix("tmux ")
            .expect("the remote command runs tmux");
        let remote = sh_arguments(remote_arguments, Some("/home/tester"));
        assert_eq!(
            remote,
            vec![
                // Ensure the session, without attaching to it yet.
                "new-session",
                "-A",
                "-d",
                "-s",
                "mult",
                "-c",
                "/home/tester/pro jects/it's",
                // One argument, so tmux runs the agent command rather than
                // reading `--resume` as a flag of its own.
                "claude --resume",
                // Each `;` is an argument, or tmux would see one command.
                ";",
                // Session-scoped, so the remote machine's own tmux
                // configuration is left alone.
                "set-option",
                "-t",
                "mult",
                "set-titles",
                "on",
                ";",
                "set-option",
                "-t",
                "mult",
                "set-titles-string",
                AGENT_TITLE_FORMAT,
                ";",
                "attach-session",
                "-t",
                "mult",
            ]
        );
    }

    /// The words `/bin/sh` splits `arguments` into, with `$HOME` set to `home`
    /// when the far side's home is what is being checked.
    fn sh_arguments(arguments: &str, home: Option<&str>) -> Vec<String> {
        let mut shell = std::process::Command::new("/bin/sh");
        shell.arg("-c").arg(format!("printf '%s\\n' {arguments}"));
        if let Some(home) = home {
            shell.env("HOME", home);
        }
        let output = shell.output().expect("run /bin/sh");
        assert!(output.status.success(), "/bin/sh rejected: {arguments}");
        String::from_utf8(output.stdout)
            .expect("printf writes the arguments back")
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    /// The branch probe reads one file, so the token it builds is the project
    /// path with `.git/HEAD` on the end — still expanded by the remote shell,
    /// still quoted against a space in the path.
    #[test]
    fn the_head_token_points_at_the_projects_own_git_directory() {
        assert_eq!(
            head_file_token("~/projects/mult").unwrap(),
            r#""$HOME/projects/mult/.git/HEAD""#
        );
        assert_eq!(
            head_file_token("/srv/work space/").unwrap(),
            r#"'/srv/work space/.git/HEAD'"#
        );
        assert_eq!(head_file_token("   "), Err(RemoteError::EmptyPath));
    }

    #[test]
    fn session_names_keep_what_tmux_accepts() {
        assert_eq!(session_name("mult"), "mult");
        assert_eq!(session_name("docs.site"), "docs-site");
        assert_eq!(session_name("my project:1"), "my-project-1");
        assert_eq!(session_name("  ...  "), "mult");
        assert_eq!(session_name(""), "mult");
    }
}
