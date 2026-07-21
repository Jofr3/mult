use std::{
    io,
    panic::{self, AssertUnwindSafe},
    sync::{atomic::AtomicBool, Arc},
};

use mult::{app::App, config, storage};
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGTERM},
    flag,
};

mod runtime;
mod terminal_guard;

use terminal_guard::TerminalGuard;

fn main() -> io::Result<()> {
    // Acquire ownership before reading mutable state and retain it until after
    // terminal cleanup. Kernel descriptor cleanup releases the lock on unwind,
    // signals, and forced process termination.
    let state_store = storage::StateStore::acquire_default()?;
    let loaded = state_store.load_or_default()?;
    if loaded.needs_save {
        // Persist migrations and first-run identities before terminal setup or
        // any runtime restoration can attach to or launch a command.
        state_store.save(&loaded.state)?;
    }
    let project = loaded.state;
    let config = config::load_or_default()?;
    let shutdown = install_shutdown_signals()?;
    let mut terminal = TerminalGuard::new(config.mouse_capture)?;

    let runtime_result = panic::catch_unwind(AssertUnwindSafe(|| {
        runtime::run(
            terminal.terminal_mut(),
            App::new(project),
            config,
            shutdown.as_ref(),
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

fn install_shutdown_signals() -> io::Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));

    for signal in [SIGINT, SIGTERM, SIGHUP] {
        // Registration order matters: on the first signal the conditional
        // action observes false, then the flag action requests graceful
        // shutdown. A second signal is an escape hatch if shutdown stalls.
        flag::register_conditional_shutdown(signal, 128 + signal, Arc::clone(&shutdown))?;
        flag::register(signal, Arc::clone(&shutdown))?;
    }

    Ok(shutdown)
}
