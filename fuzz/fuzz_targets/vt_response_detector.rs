#![no_main]

//! The terminal-response detector over arbitrary PTY output (G3).
//!
//! `mult` answers a handful of terminal queries (`CSI c`, `CSI 5 n`, `CSI 6 n`
//! and the private cursor-position form) on the child's behalf, which means it
//! runs a hand-written escape-sequence state machine over bytes a PTY child
//! chose — and a chat agent's output is not under the user's control either.
//! The machine keeps a fixed 128-byte inline CSI buffer and feeds spans to the
//! vt100 parser in batches, so the interesting failures are an index that walks
//! off that buffer and a batching span that is not a valid slice range.
//!
//! The property: no byte stream may panic, and the replies produced stay within
//! the per-chunk budget. The screen dimensions are taken from the input too,
//! since the cursor-position reply reads the cursor out of the screen.

use libfuzzer_sys::fuzz_target;

/// Mirrors `TERMINAL_MAX_RESPONSES_PER_CHUNK` in `src/pty.rs`. The longest
/// single reply is the primary-device-attributes string; 32 bytes per reply is
/// a loose upper bound on all three reply shapes, including a cursor report at
/// the largest coordinates a `u16` screen can have.
const MAX_RESPONSE_BYTES: usize = 8 * 32;

fuzz_target!(|data: &[u8]| {
    // Spend the first two bytes on the screen size so the fuzzer can steer the
    // cursor-position reply, and keep them small enough that a run does not
    // spend all its time allocating screens.
    //
    // Dimensions start at 0. This target's first two runs found a real panic
    // inside `fnug-vt100` on a one-row *or* one-column grid ("attempt to
    // subtract with overflow", `grid.rs:637` and `screen.rs:788`) from inputs as
    // ordinary as a stray non-UTF-8 byte or an emoji — reachable in `mult`,
    // because a small enough terminal window leaves a one-row output pane. That
    // was filed and fixed as A13: `PtyDimensions` now raises every size to the
    // floor the emulator survives, and `fuzz_feed_terminal_responses` goes
    // through it, so a request for a 1×1 grid exercises the clamp instead of the
    // upstream defect. The floor deliberately lives in `mult` and not here, so
    // this target keeps testing what the runtime actually does.
    let (rows, cols, bytes) = match data {
        [rows, cols, rest @ ..] => (u16::from(*rows) % 200, u16::from(*cols) % 200, rest),
        _ => return,
    };

    let responses = mult::pty::fuzz_feed_terminal_responses(rows, cols, bytes);

    assert!(
        responses.len() <= MAX_RESPONSE_BYTES,
        "per-chunk response budget exceeded: {} bytes",
        responses.len()
    );
});
