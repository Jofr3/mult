use std::{env, io, process::ExitCode};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};
use mult::{
    app::{App, StatusLevel},
    cli::{self, Binary, Invocation},
    config, model, runtime,
    storage::{self, FileStateStore, StateStore},
};

/// Exit code for a command line or a config file that cannot be honoured, as
/// opposed to a session that ran and then failed (1). Distinct so a script can
/// tell "you invoked me wrong" from "something broke".
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    // Errors are rendered here, by `Display`, and never returned from `main`:
    // returning an `io::Error` makes the runtime `Debug`-print it, which is how
    // a bad config used to greet the user with
    // `Error: Custom { kind: InvalidData, error: Error("trailing characters",
    // line: 9, column: 3) }` — a position with no filename (E5).
    let options = match cli::parse(Binary::Client, env::args_os().skip(1)) {
        Ok(Invocation::Run(options)) => options,
        Ok(Invocation::Print(text)) => {
            print!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("mult: {error}");
            eprintln!("try `mult --help`");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    // The store is built once, here, and handed to the loop. `--state` used to
    // be installed into a process global instead, because saving was a free
    // function that re-derived its path from the environment on every call
    // (F11).
    let mut store = FileStateStore::new(storage::state_path_with_override(options.state_path));
    let config_path = config::config_path_with_override(options.config_path.as_deref());
    let config = match config::load_from(&config_path) {
        Ok(config) => config,
        Err(error) => {
            // Hard failure by policy: this file names commands that are run
            // through a login shell and auto-started, so quietly falling back
            // to the defaults would run something the user did not ask for.
            eprintln!("mult: {error}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let state = match store.load() {
        Ok(state) => state,
        Err(error) => {
            eprintln!("mult: {error}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    // Starter content belongs to a genuinely new installation and nowhere else:
    // a state file that existed but could not be decoded recovers to an *empty*
    // project plus the backup notice, never to invented workspaces (F12).
    let project = if state.first_run {
        model::ProjectState::first_run_seed(env::current_dir().ok())
    } else {
        state.state
    };
    let mut app = App::new(project);
    // Startup problems that are worth continuing through are queued for the
    // status line rather than printed: stdout is about to become the TUI.
    if let Some(notice) = state.notice {
        app.push_status_notice(StatusLevel::Info, notice);
    }
    for warning in config.warnings() {
        app.push_status_notice(StatusLevel::Warning, warning);
    }

    match run_session(app, config, config_path, options.socket_path, &mut store) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mult: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_session(
    app: App,
    config: config::Config,
    config_path: std::path::PathBuf,
    socket_path: Option<std::path::PathBuf>,
    store: &mut dyn StateStore,
) -> io::Result<()> {
    let mouse_capture = config.mouse_capture;
    let mut terminal = ratatui::init();
    if let Err(error) = enable_terminal_features(mouse_capture) {
        ratatui::restore();
        return Err(error);
    }

    let result = runtime::run(
        &mut terminal,
        app,
        config,
        store,
        runtime::RuntimeOptions {
            config_path,
            socket_path,
        },
    );
    let terminal_features_result = disable_terminal_features(mouse_capture);
    ratatui::restore();
    result.and(terminal_features_result)
}

fn enable_terminal_features(mouse_capture: bool) -> io::Result<()> {
    // Preserve Shift on modified keys in terminals that support the kitty keyboard
    // protocol, so Ctrl+Shift+C does not arrive looking identical to Ctrl+C.
    if mouse_capture {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            EnableMouseCapture,
            EnableBracketedPaste,
        )
    } else {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            EnableBracketedPaste,
        )
    }
}

fn disable_terminal_features(mouse_capture: bool) -> io::Result<()> {
    if mouse_capture {
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            PopKeyboardEnhancementFlags,
        )
    } else {
        execute!(
            io::stdout(),
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags,
        )
    }
}
