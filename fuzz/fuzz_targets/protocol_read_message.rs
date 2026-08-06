//! An arbitrary byte stream through the framing reader.
//!
//! `read_message` is the first code in either process to touch bytes from the
//! socket, and it decodes a length prefix and then a `postcard` body out of
//! them. It is allowed to return any error it likes; it is not allowed to
//! panic, to allocate on an attacker's say-so (the length is capped at
//! `MAX_MESSAGE_BYTES`), or to loop.
//!
//! The stream is read repeatedly rather than once, so a body that decodes
//! successfully is followed by whatever the input says comes next — that is how
//! the daemon and the client actually use it.

#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use mult_protocol::{read_message, ClientMessage, ServerMessage};

fuzz_target!(|data: &[u8]| {
    // Both directions: the two message enums have different shapes, and each
    // side of the socket only ever decodes one of them.
    let mut reader = Cursor::new(data);
    while read_message::<ClientMessage>(&mut reader).is_ok() {}

    let mut reader = Cursor::new(data);
    while read_message::<ServerMessage>(&mut reader).is_ok() {}
});
