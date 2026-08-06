//! Arbitrary PTY output through the terminal-query responder and the emulator,
//! at every screen size a pane can ask for.
//!
//! This is the target that found A13: a pane one row or one column tall panics
//! `fnug-vt100` with "attempt to subtract with overflow" on a stray non-UTF-8
//! byte or an emoji. The dimensions below are taken from the input and pushed
//! through the same clamp production uses, so the clamped floor — the smallest
//! size the emulator is ever driven at — is exercised on every run. Resizes are
//! interleaved with output, which is what reaches A14's narrowing case.
//!
//! The bytes are split into chunks, because the responder's state machine spans
//! chunk boundaries and its answer budget is per chunk.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mult::pty::fuzz_feed_terminal_output;

/// Matches `MAX_TERMINAL_QUERY_RESPONSE_BYTES`.
const ANSWER_CAP: usize = 256;

fuzz_target!(|data: &[u8]| {
    // A header steers the geometry and the chunking; the rest is pane output.
    // Small dimensions dominate on purpose: the interesting sizes are the tiny
    // ones, and `u8` keeps every byte of the corpus meaningful.
    let Some((&chunking, rest)) = data.split_first() else {
        return;
    };
    let Some((&size_count, rest)) = rest.split_first() else {
        return;
    };

    let size_bytes = usize::from(size_count % 4 + 1) * 2;
    if rest.len() < size_bytes {
        return;
    }
    let (header, payload) = rest.split_at(size_bytes);
    // Zeros and ones are deliberately allowed through: the clamp is the thing
    // under test, and anything it lets past must survive the parser.
    let sizes: Vec<(u16, u16)> = header
        .chunks_exact(2)
        .map(|pair| (u16::from(pair[0]), u16::from(pair[1])))
        .collect();

    let chunk = usize::from(chunking).max(1);
    let chunks: Vec<&[u8]> = payload.chunks(chunk).collect();

    let answers = fuzz_feed_terminal_output(&sizes, &chunks);

    // Every answer is a terminal report, so it must be a bounded escape
    // sequence rather than anything derived from the pane's own bytes.
    for answer in answers {
        assert!(
            answer.len() <= ANSWER_CAP,
            "answer payload exceeded its cap"
        );
        assert!(
            answer.is_empty() || answer.starts_with(b"\x1b"),
            "answer payload was not an escape sequence"
        );
    }
});
