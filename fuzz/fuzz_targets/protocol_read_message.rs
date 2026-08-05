#![no_main]

//! `read_message` over an arbitrary byte stream (G3).
//!
//! Both sides of the socket call this on bytes another process wrote. The
//! socket is mode `0600` under a mode `0700` parent and the daemon checks peer
//! credentials, so the writer is always the same uid — but "same uid" is not
//! "trusted": any other program the user runs can connect and speak, and the
//! daemon's own replies reach a client that must not be crashable by them.
//!
//! The property is narrow and total: for *any* byte stream, `read_message`
//! returns `Ok` or an `Err` — it never panics, never aborts on an allocation it
//! was told to make by an attacker-chosen length header, and never runs past
//! the end of the input. Both wire types are decoded from the same bytes,
//! because the client and the daemon each parse a different one.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use mult_protocol::{ClientMessage, ServerMessage};

fuzz_target!(|data: &[u8]| {
    // The daemon's direction: bytes from a client.
    let mut reader = Cursor::new(data);
    let _ = mult_protocol::read_message::<ClientMessage>(&mut reader);

    // The client's direction: bytes from a daemon.
    let mut reader = Cursor::new(data);
    let _ = mult_protocol::read_message::<ServerMessage>(&mut reader);

    // A framed stream is a sequence, not one message: keep reading until the
    // reader stops making progress, so a payload that decodes cleanly but
    // leaves the cursor mid-frame is exercised too.
    let mut reader = Cursor::new(data);
    for _ in 0..16 {
        if mult_protocol::read_message::<ClientMessage>(&mut reader).is_err() {
            break;
        }
    }
});
