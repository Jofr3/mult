//! Command-line parsing for `mult` and `mult-server`.
//!
//! Neither binary looked at `argv` at all: `mult --version` started the TUI and
//! `mult --socekt /path` started it too, silently ignoring the typo and talking
//! to the wrong daemon (E1). Both now parse first and run second.
//!
//! Hand-rolled on purpose. The surface is three path options plus help and
//! version, and `AGENTS.md` says not to add a dependency that does not clearly
//! reduce risk; `clap` would be the largest dependency in the tree for a parser
//! that fits on two screens. [`parse`] is a pure function of its arguments, so
//! the tests below drive it directly instead of spawning processes.

use std::{
    ffi::{OsStr, OsString},
    fmt,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};

/// Which binary's argument grammar to apply. They differ: the daemon owns no
/// config and no state, so it *rejects* `--config`/`--state` rather than
/// accepting a flag that would do nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binary {
    Client,
    Server,
}

impl Binary {
    pub fn name(self) -> &'static str {
        match self {
            Self::Client => "mult",
            Self::Server => "mult-server",
        }
    }
}

/// Paths chosen on the command line. `None` means "fall through to the
/// environment variable, and then to the default"; every path here is the
/// highest-precedence source for the file it names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub config: Option<PathBuf>,
    pub state: Option<PathBuf>,
    pub socket: Option<PathBuf>,
}

/// What the caller should do with a successfully parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Start normally with these options.
    Run(Options),
    /// Write this text to stdout and exit successfully. `--help` and
    /// `--version` are the whole program when they are present.
    Print(String),
}

/// A command line that could not be run. Rendered with the binary name and a
/// pointer at `--help`, the way a user expects a CLI to fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
    binary: Binary,
    message: String,
}

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {}\nTry `{} --help` for the accepted options.",
            self.binary.name(),
            self.message,
            self.binary.name()
        )
    }
}

impl std::error::Error for UsageError {}

/// Parses one binary's arguments, *excluding* the program name.
///
/// Both `--flag value` and `--flag=value` are accepted. A value may be any OS
/// string — paths are not required to be UTF-8 — so the split happens on bytes
/// and only the flag name itself has to be text. Anything unrecognized, and any
/// positional argument, is an error: silently starting a TUI after ignoring
/// what the user typed is the defect this module exists to fix.
pub fn parse<I>(binary: Binary, arguments: I) -> Result<Invocation, UsageError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut options = Options::default();
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        let (name, inline) = split_inline_value(&argument);
        let Some(name) = name.to_str() else {
            return Err(unrecognized(binary, &argument));
        };

        match name {
            "-h" | "--help" => return Ok(Invocation::Print(help_text(binary))),
            "-V" | "--version" => return Ok(Invocation::Print(version_text(binary))),
            // The end-of-options marker is accepted so `--` alone is not an
            // error, but nothing may follow it: neither binary takes operands.
            "--" => {
                if let Some(operand) = arguments.next() {
                    return Err(unexpected_operand(binary, &operand));
                }
            }
            "--config" | "--state" | "--socket" => {
                let slot = match (binary, name) {
                    (_, "--socket") => &mut options.socket,
                    (Binary::Client, "--config") => &mut options.config,
                    (Binary::Client, "--state") => &mut options.state,
                    (Binary::Server, _) => return Err(not_for_the_daemon(binary, name)),
                    _ => unreachable!("the match above enumerates every accepted flag"),
                };
                let value = match inline {
                    Some(value) => value,
                    None => arguments
                        .next()
                        .ok_or_else(|| missing_value(binary, name))?,
                };
                assign(binary, name, slot, value)?;
            }
            _ if name.starts_with('-') => return Err(unrecognized(binary, &argument)),
            _ => return Err(unexpected_operand(binary, &argument)),
        }
    }

    Ok(Invocation::Run(options))
}

/// `--flag=value` split into its two halves, on bytes so a non-UTF-8 value
/// survives. Anything that is not a long flag carrying an `=` is returned whole.
fn split_inline_value(argument: &OsStr) -> (OsString, Option<OsString>) {
    let bytes = argument.as_bytes();
    if !bytes.starts_with(b"--") {
        return (argument.to_os_string(), None);
    }
    match bytes.iter().position(|byte| *byte == b'=') {
        Some(equals) if equals > 2 => (
            OsString::from_vec(bytes[..equals].to_vec()),
            Some(OsString::from_vec(bytes[equals + 1..].to_vec())),
        ),
        _ => (argument.to_os_string(), None),
    }
}

fn assign(
    binary: Binary,
    name: &str,
    slot: &mut Option<PathBuf>,
    value: OsString,
) -> Result<(), UsageError> {
    if slot.is_some() {
        return Err(error(binary, format!("`{name}` was given more than once")));
    }
    if value.is_empty() {
        return Err(error(binary, format!("`{name}` requires a path")));
    }
    *slot = Some(PathBuf::from(value));
    Ok(())
}

fn error(binary: Binary, message: String) -> UsageError {
    UsageError { binary, message }
}

fn unrecognized(binary: Binary, argument: &OsStr) -> UsageError {
    error(
        binary,
        format!("unrecognized option `{}`", argument.to_string_lossy()),
    )
}

fn unexpected_operand(binary: Binary, argument: &OsStr) -> UsageError {
    error(
        binary,
        format!(
            "unexpected argument `{}`; {} takes options only",
            argument.to_string_lossy(),
            binary.name()
        ),
    )
}

fn missing_value(binary: Binary, name: &str) -> UsageError {
    error(binary, format!("`{name}` requires a path"))
}

fn not_for_the_daemon(binary: Binary, name: &str) -> UsageError {
    error(
        binary,
        format!(
            "`{name}` is a mult option, not a mult-server one: the daemon reads neither the config nor the state file"
        ),
    )
}

pub fn version_text(binary: Binary) -> String {
    format!("{} {}", binary.name(), env!("CARGO_PKG_VERSION"))
}

pub fn help_text(binary: Binary) -> String {
    match binary {
        Binary::Client => format!(
            "\
{version}
A terminal UI for parallel agent chats and shell sessions.

Usage:
  mult [OPTIONS]

Options:
      --config <PATH>  Read configuration from PATH instead of $MULT_CONFIG_PATH
                       or $XDG_CONFIG_HOME/mult/config.json
      --state <PATH>   Read and write session state at PATH instead of
                       $MULT_STATE_PATH or $XDG_DATA_HOME/mult/state.json
      --socket <PATH>  Reach mult-server at PATH instead of $MULT_SOCKET_PATH
                       or $XDG_RUNTIME_DIR/mult.sock
  -h, --help           Print this help and exit
  -V, --version        Print version information and exit

Each path option overrides its environment variable, which overrides the
default: flag > environment > default. `--flag value` and `--flag=value` are
equivalent.

Configuration reference: docs/CONFIG.md
Failure modes:           docs/TROUBLESHOOTING.md",
            version = version_text(binary)
        ),
        Binary::Server => format!(
            "\
{version}
The persistent PTY daemon behind mult. `mult` autospawns it when the binary sits
next to the client, so it usually does not need to be started by hand.

Usage:
  mult-server [OPTIONS]

Options:
      --socket <PATH>  Bind the daemon socket at PATH instead of
                       $MULT_SOCKET_PATH or $XDG_RUNTIME_DIR/mult.sock
  -h, --help           Print this help and exit
  -V, --version        Print version information and exit

The socket path overrides $MULT_SOCKET_PATH, which overrides the default: flag >
environment > default. Both ends must agree on it.

`--config` and `--state` belong to the client; the daemon reads neither.

Daemon and protocol notes: docs/DAEMON.md",
            version = version_text(binary)
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn parse_client(arguments: &[&str]) -> Result<Invocation, UsageError> {
        parse(Binary::Client, arguments.iter().map(OsString::from))
    }

    fn parse_server(arguments: &[&str]) -> Result<Invocation, UsageError> {
        parse(Binary::Server, arguments.iter().map(OsString::from))
    }

    fn options(arguments: &[&str]) -> Options {
        match parse_client(arguments).expect("arguments parse") {
            Invocation::Run(options) => options,
            Invocation::Print(text) => panic!("expected options, printed {text}"),
        }
    }

    #[test]
    fn no_arguments_run_with_every_path_left_to_the_environment() {
        assert_eq!(options(&[]), Options::default());
        assert_eq!(parse_server(&[]), Ok(Invocation::Run(Options::default())));
    }

    #[test]
    fn path_options_accept_both_separated_and_inline_values() {
        let separated = options(&[
            "--config",
            "/etc/mult.json",
            "--state",
            "/var/state.json",
            "--socket",
            "/run/mult.sock",
        ]);
        let inline = options(&[
            "--config=/etc/mult.json",
            "--state=/var/state.json",
            "--socket=/run/mult.sock",
        ]);

        assert_eq!(separated, inline);
        assert_eq!(
            separated.config.as_deref(),
            Some(Path::new("/etc/mult.json"))
        );
        assert_eq!(
            separated.state.as_deref(),
            Some(Path::new("/var/state.json"))
        );
        assert_eq!(
            separated.socket.as_deref(),
            Some(Path::new("/run/mult.sock"))
        );
    }

    /// A value is a path, not text: `--config=$'\xff'` must survive rather than
    /// being replaced by U+FFFD or rejected.
    #[test]
    fn a_value_that_is_not_utf8_is_kept_verbatim() {
        let argument = OsString::from_vec(b"--config=/tmp/\xff.json".to_vec());
        let Invocation::Run(options) = parse(Binary::Client, [argument]).expect("parse") else {
            panic!("expected options");
        };

        assert_eq!(
            options
                .config
                .as_deref()
                .map(|path| path.as_os_str().as_bytes()),
            Some(&b"/tmp/\xff.json"[..])
        );
    }

    #[test]
    fn help_and_version_are_printed_and_win_over_everything_else() {
        for flag in ["-h", "--help"] {
            let Invocation::Print(text) = parse_client(&[flag, "--nonsense"]).expect("help parses")
            else {
                panic!("`{flag}` must print");
            };
            assert!(text.starts_with("mult 0."), "{text}");
            assert!(text.contains("Usage:\n  mult [OPTIONS]"), "{text}");
            assert!(text.contains("--config <PATH>"), "{text}");
            assert!(text.contains("flag > environment > default"), "{text}");
        }

        for flag in ["-V", "--version"] {
            assert_eq!(
                parse_client(&[flag]),
                Ok(Invocation::Print(format!(
                    "mult {}",
                    env!("CARGO_PKG_VERSION")
                )))
            );
            assert_eq!(
                parse_server(&[flag]),
                Ok(Invocation::Print(format!(
                    "mult-server {}",
                    env!("CARGO_PKG_VERSION")
                )))
            );
        }
    }

    #[test]
    fn the_daemon_help_offers_only_the_socket() {
        let Invocation::Print(text) = parse_server(&["--help"]).expect("help parses") else {
            panic!("expected help");
        };

        assert!(text.contains("--socket <PATH>"), "{text}");
        assert!(!text.contains("--config <PATH>"), "{text}");
        assert!(!text.contains("--state <PATH>"), "{text}");
    }

    /// The E1 defect in one assertion: a mistyped flag used to launch the TUI.
    #[test]
    fn an_unknown_flag_is_an_error_and_never_a_silent_launch() {
        let error = parse_client(&["--socekt", "/run/mult.sock"]).expect_err("typo must not run");

        assert!(
            error.to_string().contains("unrecognized option `--socekt`"),
            "{error}"
        );
        assert!(error.to_string().contains("Try `mult --help`"), "{error}");
        assert!(parse_client(&["-x"]).is_err());
        assert!(parse_client(&["--config-file=/tmp/x"]).is_err());
    }

    #[test]
    fn a_stray_positional_is_an_error() {
        let error = parse_client(&["workspace"]).expect_err("operands are not accepted");
        assert!(
            error
                .to_string()
                .contains("unexpected argument `workspace`"),
            "{error}"
        );

        // `--` ends the options; it is harmless alone and fatal with an operand.
        assert_eq!(
            parse_client(&["--"]),
            Ok(Invocation::Run(Options::default()))
        );
        assert!(parse_client(&["--", "workspace"]).is_err());
    }

    #[test]
    fn a_flag_without_a_value_or_given_twice_is_an_error() {
        assert!(parse_client(&["--config"])
            .expect_err("a dangling flag is an error")
            .to_string()
            .contains("`--config` requires a path"));
        assert!(parse_client(&["--config="])
            .expect_err("an empty value is an error")
            .to_string()
            .contains("`--config` requires a path"));
        assert!(parse_client(&["--state", "/a", "--state", "/b"])
            .expect_err("a repeated flag is an error")
            .to_string()
            .contains("`--state` was given more than once"));
    }

    /// The daemon must reject what it cannot honour instead of accepting a
    /// no-op flag: `mult-server --config x` looked like it worked.
    #[test]
    fn the_daemon_rejects_client_only_options() {
        for flag in ["--config", "--state"] {
            let error = parse_server(&[flag, "/tmp/x"])
                .expect_err("the daemon must not accept a client option");
            assert!(
                error.to_string().contains("not a mult-server one"),
                "{error}"
            );
        }

        assert_eq!(
            parse_server(&["--socket", "/run/mult.sock"]),
            Ok(Invocation::Run(Options {
                socket: Some(PathBuf::from("/run/mult.sock")),
                ..Options::default()
            }))
        );
    }
}
