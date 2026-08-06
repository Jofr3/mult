//! Argv parsing for the `mult` client and the `mult-server` daemon.
//!
//! Hand-rolled on purpose: the surface is five flags with no subcommands, and
//! `clap` would be a new dependency (see `AGENTS.md`). [`parse`] is a pure
//! function over an iterator of arguments — it never reads the environment and
//! never exits — so precedence, rejection and the help text are all testable
//! without spawning a process.
//!
//! Precedence: a flag always wins over the corresponding environment variable,
//! which in turn wins over the built-in default path.

use std::{ffi::OsString, fmt, path::PathBuf};

/// Which binary's flag set to accept.
///
/// The two share `--help`/`--version` but not the file overrides: the daemon
/// owns no config and no state file, so accepting `--config` there would be a
/// flag that silently does nothing — the same failure mode `deny_unknown_fields`
/// removes from `config.json`. An unsupported-but-known flag is therefore
/// rejected like any other unknown option, with the supported set listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binary {
    Client,
    Server,
}

impl Binary {
    /// The name the binary is invoked as, used in usage and version output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Client => "mult",
            Self::Server => "mult-server",
        }
    }

    fn accepts_config(self) -> bool {
        self == Self::Client
    }

    fn accepts_state(self) -> bool {
        self == Self::Client
    }
}

/// Path overrides collected from argv. `None` means "fall back to the
/// environment variable, then to the default path".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Options {
    pub config_path: Option<PathBuf>,
    pub state_path: Option<PathBuf>,
    pub socket_path: Option<PathBuf>,
}

/// What the caller should do with a successfully parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Start normally with these overrides.
    Run(Options),
    /// Write this text to stdout and exit successfully (`--help`, `--version`).
    Print(String),
}

/// A command line that cannot be honoured. Always a nonzero exit, never a
/// silent fallback to launching the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// An option this binary does not accept.
    UnknownOption { binary: Binary, option: String },
    /// A recognised option whose value is missing.
    MissingValue { option: &'static str },
    /// A positional argument; neither binary takes any.
    UnexpectedArgument { argument: String },
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption { binary, option } => write!(
                formatter,
                "unknown option `{option}`; supported options are {}",
                supported_options(*binary)
            ),
            Self::MissingValue { option } => {
                write!(formatter, "option `{option}` requires a path")
            }
            Self::UnexpectedArgument { argument } => {
                write!(formatter, "unexpected argument `{argument}`")
            }
        }
    }
}

impl std::error::Error for CliError {}

/// Parse `args` — the arguments *after* argv[0] — for `binary`.
pub fn parse<I, S>(binary: Binary, args: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut options = Options::default();
    let mut args = args.into_iter().map(Into::into).peekable();

    while let Some(argument) = args.next() {
        // Flags are ASCII, so a non-UTF-8 argument can only be a value or a
        // stray positional; matching on the lossy form keeps the parser off
        // platform-specific `OsStr` byte access without losing non-UTF-8 paths,
        // which still arrive intact in the separated (`--config <path>`) form.
        let Some(text) = argument.to_str() else {
            return Err(CliError::UnexpectedArgument {
                argument: argument.to_string_lossy().into_owned(),
            });
        };

        match text {
            "--help" | "-h" => return Ok(Invocation::Print(help_text(binary))),
            "--version" | "-V" => return Ok(Invocation::Print(version_text(binary))),
            // Nothing takes positional arguments, so `--` can only introduce
            // something this binary would reject anyway.
            "--" => {
                if let Some(argument) = args.next() {
                    return Err(CliError::UnexpectedArgument {
                        argument: argument.to_string_lossy().into_owned(),
                    });
                }
                break;
            }
            _ => {}
        }

        let (name, inline_value) = match text.split_once('=') {
            Some((name, value)) => (name, Some(OsString::from(value))),
            None => (text, None),
        };

        let slot = match name {
            "--config" if binary.accepts_config() => &mut options.config_path,
            "--state" if binary.accepts_state() => &mut options.state_path,
            "--socket" => &mut options.socket_path,
            _ if name.starts_with('-') => {
                return Err(CliError::UnknownOption {
                    binary,
                    option: name.to_string(),
                })
            }
            _ => {
                return Err(CliError::UnexpectedArgument {
                    argument: text.to_string(),
                })
            }
        };

        let value = match inline_value {
            Some(value) => value,
            None => args.next().ok_or(CliError::MissingValue {
                // `name` is one of the three literals matched just above.
                option: match name {
                    "--config" => "--config",
                    "--state" => "--state",
                    _ => "--socket",
                },
            })?,
        };
        // A later occurrence wins, matching how every other option parser
        // behaves and how the shell's own last-assignment-wins reads.
        *slot = Some(PathBuf::from(value));
    }

    Ok(Invocation::Run(options))
}

fn supported_options(binary: Binary) -> &'static str {
    match binary {
        Binary::Client => "--help, --version, --config, --state, --socket",
        Binary::Server => "--help, --version, --socket",
    }
}

/// `name version`, without a line ending — the form embedded in the help
/// header. [`version_text`] is what `--version` actually prints.
fn version_line(binary: Binary) -> String {
    format!("{} {}", binary.name(), env!("CARGO_PKG_VERSION"))
}

/// Exactly what `--version` writes to stdout, newline included: an
/// [`Invocation::Print`] payload is printed verbatim.
pub fn version_text(binary: Binary) -> String {
    format!("{}\n", version_line(binary))
}

pub fn help_text(binary: Binary) -> String {
    let mut text = format!("{}\n\n", version_line(binary));
    match binary {
        Binary::Client => text.push_str(
            "A terminal UI for multiplexing project workspaces, agent chats and terminals.\n\n\
             Usage: mult [OPTIONS]\n\n\
             Options:\n  \
               -h, --help           Print this help and exit\n  \
               -V, --version        Print the version and exit\n      \
                   --config <PATH>  Config file to read (overrides $MULT_CONFIG_PATH)\n      \
                   --state <PATH>   State file to read and write (overrides $MULT_STATE_PATH)\n      \
                   --socket <PATH>  mult-server socket to use (overrides $MULT_SOCKET_PATH)\n",
        ),
        Binary::Server => text.push_str(
            "The persistent PTY daemon the mult client connects to.\n\n\
             Usage: mult-server [OPTIONS]\n\n\
             Options:\n  \
               -h, --help           Print this help and exit\n  \
               -V, --version        Print the version and exit\n      \
                   --socket <PATH>  Socket to listen on (overrides $MULT_SOCKET_PATH)\n",
        ),
    }
    text.push_str("\nFlags take precedence over the environment variables they name.\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(binary: Binary, args: &[&str]) -> Options {
        match parse(binary, args.iter().copied()).expect("parse succeeds") {
            Invocation::Run(options) => options,
            Invocation::Print(text) => panic!("expected a run, got output: {text}"),
        }
    }

    #[test]
    fn no_arguments_leaves_every_path_to_the_environment() {
        assert_eq!(run(Binary::Client, &[]), Options::default());
        assert_eq!(run(Binary::Server, &[]), Options::default());
    }

    #[test]
    fn path_flags_are_collected_in_both_forms() {
        let separated = run(
            Binary::Client,
            &[
                "--config",
                "/tmp/c.json",
                "--state",
                "/tmp/s.json",
                "--socket",
                "/tmp/m.sock",
            ],
        );
        let inline = run(
            Binary::Client,
            &[
                "--config=/tmp/c.json",
                "--state=/tmp/s.json",
                "--socket=/tmp/m.sock",
            ],
        );

        assert_eq!(separated, inline);
        assert_eq!(separated.config_path, Some(PathBuf::from("/tmp/c.json")));
        assert_eq!(separated.state_path, Some(PathBuf::from("/tmp/s.json")));
        assert_eq!(separated.socket_path, Some(PathBuf::from("/tmp/m.sock")));
    }

    #[test]
    fn a_flag_overrides_the_matching_environment_variable() {
        // The parser never reads the environment; precedence is expressed by
        // the resolver, so this pins both halves of the contract: a flag
        // produces `Some`, and its absence produces the `None` that lets the
        // environment through.
        let with_flag = run(Binary::Client, &["--config", "/tmp/from-flag.json"]);
        let without_flag = run(Binary::Client, &[]);

        assert_eq!(
            crate::config::config_path_from(
                with_flag.config_path.as_deref(),
                Some(std::ffi::OsStr::new("/tmp/from-env.json")),
                std::path::Path::new("/home/example/.config"),
            ),
            (
                PathBuf::from("/tmp/from-flag.json"),
                crate::config::ConfigSource::Explicit
            )
        );
        assert_eq!(
            crate::config::config_path_from(
                without_flag.config_path.as_deref(),
                Some(std::ffi::OsStr::new("/tmp/from-env.json")),
                std::path::Path::new("/home/example/.config"),
            ),
            (
                PathBuf::from("/tmp/from-env.json"),
                crate::config::ConfigSource::Explicit
            )
        );
    }

    #[test]
    fn the_last_occurrence_of_a_flag_wins() {
        let options = run(Binary::Client, &["--state", "/tmp/a", "--state", "/tmp/b"]);

        assert_eq!(options.state_path, Some(PathBuf::from("/tmp/b")));
    }

    #[test]
    fn unknown_flags_are_rejected_rather_than_ignored() {
        // The whole point of E1: an unrecognised flag must never fall through
        // to "launch the TUI anyway".
        assert_eq!(
            parse(Binary::Client, ["--bogus"]),
            Err(CliError::UnknownOption {
                binary: Binary::Client,
                option: "--bogus".to_string(),
            })
        );
        assert_eq!(
            parse(Binary::Client, ["-x"]),
            Err(CliError::UnknownOption {
                binary: Binary::Client,
                option: "-x".to_string(),
            })
        );
        let message = parse(Binary::Client, ["--bogus"])
            .expect_err("unknown flag")
            .to_string();
        assert!(message.contains("unknown option `--bogus`"), "{message}");
        assert!(message.contains("--config"), "{message}");
    }

    #[test]
    fn positional_arguments_are_rejected() {
        assert_eq!(
            parse(Binary::Client, ["workspace"]),
            Err(CliError::UnexpectedArgument {
                argument: "workspace".to_string(),
            })
        );
        assert_eq!(
            parse(Binary::Client, ["--", "workspace"]),
            Err(CliError::UnexpectedArgument {
                argument: "workspace".to_string(),
            })
        );
    }

    #[test]
    fn a_flag_without_its_value_is_an_error() {
        assert_eq!(
            parse(Binary::Client, ["--config"]),
            Err(CliError::MissingValue { option: "--config" })
        );
        assert_eq!(
            parse(Binary::Server, ["--socket"]),
            Err(CliError::MissingValue { option: "--socket" })
        );
    }

    #[test]
    fn the_daemon_rejects_the_client_only_flags() {
        // Accepting `--config` here and doing nothing with it is exactly the
        // silent no-op E6 removes from the config file itself.
        assert_eq!(
            parse(Binary::Server, ["--config", "/tmp/c.json"]),
            Err(CliError::UnknownOption {
                binary: Binary::Server,
                option: "--config".to_string(),
            })
        );
        assert_eq!(
            run(Binary::Server, &["--socket", "/tmp/m.sock"]).socket_path,
            Some(PathBuf::from("/tmp/m.sock"))
        );
    }

    #[test]
    fn help_and_version_print_and_stop_parsing() {
        for flag in ["--help", "-h"] {
            let Invocation::Print(text) = parse(Binary::Client, [flag, "--bogus"])
                .expect("help wins over the rest of the line")
            else {
                panic!("expected help output");
            };
            assert!(text.starts_with("mult "), "{text}");
            assert!(text.contains("Usage: mult [OPTIONS]"), "{text}");
            assert!(text.contains("--socket <PATH>"), "{text}");
            assert!(text.contains("precedence"), "{text}");
        }

        for flag in ["--version", "-V"] {
            assert_eq!(
                parse(Binary::Client, [flag]),
                Ok(Invocation::Print(format!(
                    "mult {}\n",
                    env!("CARGO_PKG_VERSION")
                )))
            );
            assert_eq!(
                parse(Binary::Server, [flag]),
                Ok(Invocation::Print(format!(
                    "mult-server {}\n",
                    env!("CARGO_PKG_VERSION")
                )))
            );
        }
    }

    #[test]
    fn the_daemon_help_only_advertises_what_it_accepts() {
        let text = help_text(Binary::Server);

        assert!(text.contains("Usage: mult-server [OPTIONS]"), "{text}");
        assert!(text.contains("--socket <PATH>"), "{text}");
        assert!(!text.contains("--config"), "{text}");
        assert!(!text.contains("--state"), "{text}");
    }
}
