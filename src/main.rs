use std::{
    env, io,
    panic::{self, AssertUnwindSafe},
    process::{self, ExitCode},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use mult::{
    app::{App, NoticeLevel, NoticeSource},
    cli::{self, Binary, Invocation, Options},
    config, runtime, storage,
};
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGTERM},
    flag,
};

mod terminal_guard;

use terminal_guard::TerminalGuard;

/// Exit status for a command line that could not be run, distinct from a
/// failure while running.
const USAGE_EXIT_CODE: u8 = 2;

fn main() -> ExitCode {
    let options = match cli::parse(Binary::Client, env::args_os().skip(1)) {
        Ok(Invocation::Run(options)) => options,
        Ok(Invocation::Print(text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(USAGE_EXIT_CODE);
        }
    };

    match run(options) {
        Ok(()) => ExitCode::SUCCESS,
        // `Display`, never `Debug`: returning `io::Result` from `main` printed
        // bad config as `Error: Os { code: 40, kind: FilesystemLoop, … }`, which
        // named neither the file nor what to do about it (E5).
        Err(error) => {
            eprintln!("mult: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(options: Options) -> io::Result<()> {
    if let Some(socket) = options.socket {
        // The client resolves its socket inside `PtyRuntime`, so `--socket` is
        // installed where that resolution happens rather than threaded through
        // the runtime. Done first, and once, so nothing can resolve one before
        // the override is in place.
        mult_protocol::set_socket_path_override(socket)
            .map_err(|path| io::Error::other(format!("socket path already fixed: {path:?}")))?;
    }

    // Config first: it is read-only, so a rejected one fails without having
    // taken the state lock or touched the state file. Reversing these two makes
    // `mult --config <broken>` report whatever the state path had to say
    // instead of the config error the user is looking for.
    let config = config::load_or_default(options.config.as_deref())?;

    // Acquire ownership before reading mutable state and retain it until after
    // terminal cleanup. Kernel descriptor cleanup releases the lock on unwind,
    // signals, and forced process termination.
    let state_store =
        storage::StateStore::acquire(storage::StatePaths::resolve_with(options.state.as_deref())?)?;
    let loaded = state_store.load_or_default()?;
    if loaded.needs_save {
        // Persist migrations and first-run identities before terminal setup or
        // any runtime restoration can attach to or launch a command.
        state_store.save(&loaded.state)?;
    }
    let project = loaded.state;

    // Also printed to stderr, where they are legible before the alternate
    // screen opens and again once it closes. The status surface carries them
    // during the session: config warnings are re-derived by `runtime::run` from
    // the `Config` it owns (so a reload refreshes them), while the state notice
    // describes something that happened before the app existed and is pushed
    // once, here.
    if let Some(notice) = &loaded.notice {
        eprintln!("mult: {notice}");
    }
    for warning in config.warnings() {
        eprintln!("mult: {warning}");
    }

    let mut app = App::new(project);
    if let Some(notice) = loaded.notice {
        app.push_notice(NoticeLevel::Warning, NoticeSource::Report, notice);
    }

    let shutdown = install_shutdown_signals()?;
    let mut terminal = TerminalGuard::new(config.mouse_capture)?;

    let runtime_result = panic::catch_unwind(AssertUnwindSafe(|| {
        runtime::run(
            terminal.terminal_mut(),
            app,
            config,
            options.config.as_deref(),
            shutdown.as_ref(),
            &state_store,
        )
    }));
    let cleanup_result = terminal.cleanup();

    match runtime_result {
        Ok(Ok(())) => cleanup_result,
        Ok(Err(runtime_error)) => {
            if let Err(cleanup_error) = cleanup_result {
                eprintln!("terminal cleanup after runtime error also failed: {cleanup_error}");
            }
            Err(runtime_error)
        }
        Err(payload) => {
            if let Err(cleanup_error) = cleanup_result {
                eprintln!("terminal cleanup while unwinding also failed: {cleanup_error}");
            }
            panic::resume_unwind(payload)
        }
    }
}

/// How long a requested shutdown may take before it is taken out of the event
/// loop's hands. Generous: the loop acts on the flag within one ~16 ms tick, and
/// everything after it is a state checkpoint and a terminal restore.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

fn install_shutdown_signals() -> io::Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));

    for signal in [SIGINT, SIGTERM, SIGHUP] {
        // Registration order matters: on the first signal the conditional
        // action observes false, then the flag action requests graceful
        // shutdown. A second signal is an escape hatch if shutdown stalls.
        flag::register_conditional_shutdown(signal, 128 + signal, Arc::clone(&shutdown))?;
        flag::register(signal, Arc::clone(&shutdown))?;
    }

    watch_shutdown(Arc::clone(&shutdown));
    Ok(shutdown)
}

/// Guarantee that a shutdown signal ends the process, whether or not the event
/// loop is in a position to notice it.
///
/// The escape hatch above needs a *second* signal, and a closed window only ever
/// sends one. That was survivable for as long as the loop was certain to come
/// back round to the flag — which is exactly what a hung-up terminal took away
/// (see `runtime::poll_host_terminal`): the client stayed up at 100% CPU holding
/// the state lock and every pane lease, and the agents it had launched could not
/// be reached by any replacement client. A wedged shutdown must cost at most an
/// unsaved checkpoint, so it is bounded here rather than trusted anywhere.
fn watch_shutdown(shutdown: Arc<AtomicBool>) {
    thread::spawn(move || {
        while !shutdown.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(50));
        }
        thread::sleep(SHUTDOWN_GRACE);
        // Still here: the loop never acted on the flag. `process::exit` runs no
        // destructors, which is the point — whatever is holding it up is not
        // something to wait on, and the descriptors that matter (the state lock,
        // the daemon socket) are released by the kernel.
        process::exit(128 + SIGHUP);
    });
}
