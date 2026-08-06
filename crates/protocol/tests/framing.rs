//! Wire-framing tests for `read_message` / `write_message` (G2).
//!
//! The unit tests in `lib.rs` cover an oversize length prefix and trailing
//! bytes inside a frame. The cases here are the ones a `UnixStream` actually
//! produces and a `&[u8]` reader never does:
//!
//! - a frame cut short by a peer that died mid-write (`UnexpectedEof`);
//! - a `len == 0` frame, which carries no payload to decode (`InvalidData`);
//! - a payload delivered a byte at a time, because a socket has no obligation
//!   to hand over 8 KiB in one `read` — `read_exact` must reassemble it;
//! - back-to-back frames in one stream, which must decode independently and
//!   leave the reader positioned exactly at the next length prefix;
//! - malformed postcard bodies, which must always come back as an error and
//!   must never panic. The generator for those is deterministic and seeded by
//!   hand: no `proptest`, so a failure reproduces from the printed seed alone.

use std::io::{self, Read};

use mult_protocol::{
    read_message, write_message, ClientMessage, SessionId, SessionIdentity, SessionToken,
    StateNamespace, MAX_MESSAGE_BYTES,
};

/// A reader that yields at most `chunk` bytes per `read`, so `read_exact` has
/// to loop. `chunk == 1` is the pathological socket: one byte per syscall.
struct ChunkedReader<'a> {
    bytes: &'a [u8],
    chunk: usize,
}

impl<'a> ChunkedReader<'a> {
    fn new(bytes: &'a [u8], chunk: usize) -> Self {
        Self { bytes, chunk }
    }
}

impl Read for ChunkedReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let take = self.bytes.len().min(output.len()).min(self.chunk);
        output[..take].copy_from_slice(&self.bytes[..take]);
        self.bytes = &self.bytes[take..];
        Ok(take)
    }
}

/// A reader that returns `Interrupted` before every successful read, which is
/// what a signal-heavy process does to a blocking socket. `read_exact` is
/// documented to retry on `Interrupted`; this pins that it actually does.
struct InterruptingReader<'a> {
    bytes: &'a [u8],
    interrupt_next: bool,
}

impl Read for InterruptingReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.interrupt_next {
            self.interrupt_next = false;
            return Err(io::Error::new(io::ErrorKind::Interrupted, "signal"));
        }
        self.interrupt_next = true;
        let take = self.bytes.len().min(output.len()).min(3);
        output[..take].copy_from_slice(&self.bytes[..take]);
        self.bytes = &self.bytes[take..];
        Ok(take)
    }
}

fn identity() -> SessionIdentity {
    SessionIdentity {
        namespace: StateNamespace::from_bytes([0x11; 16]).expect("non-zero namespace"),
        token: SessionToken::from_bytes([0x22; 16]).expect("non-zero token"),
    }
}

fn sample_message(session: u32) -> ClientMessage {
    ClientMessage::Attach {
        request_id: mult_protocol::RequestId::new(u64::from(session) + 1).expect("non-zero"),
        identity: identity(),
        session: SessionId(session),
        rows: 24,
        cols: 80,
    }
}

fn encode(message: &ClientMessage) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_message(&mut bytes, message).expect("encode message");
    bytes
}

#[test]
fn a_frame_cut_short_reports_unexpected_eof() {
    let frame = encode(&sample_message(1));
    assert!(frame.len() > 5, "the fixture must have a real payload");

    for truncated_at in [1, 2, 3, 4, 5, frame.len() - 1] {
        let error = read_message::<ClientMessage>(&mut &frame[..truncated_at])
            .expect_err("a truncated frame must not decode");

        assert_eq!(
            error.kind(),
            io::ErrorKind::UnexpectedEof,
            "truncating at {truncated_at} bytes"
        );
    }
}

#[test]
fn an_empty_stream_reports_unexpected_eof_rather_than_a_decode_error() {
    let error = read_message::<ClientMessage>(&mut io::empty())
        .expect_err("no length prefix at all must not decode");

    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn a_zero_length_frame_is_invalid_data() {
    let frame = 0_u32.to_be_bytes();

    let error = read_message::<ClientMessage>(&mut frame.as_slice())
        .expect_err("an empty payload decodes to no message");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn a_payload_split_across_reads_is_reassembled() {
    let message = sample_message(9);
    let frame = encode(&message);

    for chunk in [1, 2, 3, 5, 7, frame.len() - 1, frame.len()] {
        let mut reader = ChunkedReader::new(&frame, chunk);

        let decoded: ClientMessage =
            read_message(&mut reader).expect("a split payload must still decode");

        assert_eq!(decoded, message, "with {chunk} bytes per read");
    }
}

#[test]
fn interrupted_reads_are_retried_rather_than_reported() {
    let message = sample_message(4);
    let frame = encode(&message);
    let mut reader = InterruptingReader {
        bytes: &frame,
        interrupt_next: true,
    };

    let decoded: ClientMessage = read_message(&mut reader).expect("EINTR must not fail a frame");

    assert_eq!(decoded, message);
}

#[test]
fn back_to_back_frames_decode_independently_from_one_stream() {
    let messages = [sample_message(1), sample_message(2), sample_message(3)];
    let mut stream = Vec::new();
    for message in &messages {
        write_message(&mut stream, message).expect("encode message");
    }
    // One byte per read across the whole stream: every frame boundary is
    // therefore crossed mid-`read_exact`, which is the case a `&[u8]` reader
    // can never produce.
    let mut reader = ChunkedReader::new(&stream, 1);

    for expected in &messages {
        let decoded: ClientMessage = read_message(&mut reader).expect("decode framed message");
        assert_eq!(&decoded, expected);
    }

    let error = read_message::<ClientMessage>(&mut reader)
        .expect_err("the stream holds exactly three frames");
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn a_truncated_second_frame_does_not_corrupt_the_first() {
    let first = sample_message(1);
    let mut stream = encode(&first);
    let second = encode(&sample_message(2));
    stream.extend_from_slice(&second[..second.len() - 2]);
    let mut reader = ChunkedReader::new(&stream, 1);

    let decoded: ClientMessage = read_message(&mut reader).expect("the first frame is complete");
    assert_eq!(decoded, first);

    let error = read_message::<ClientMessage>(&mut reader).expect_err("the second frame is short");
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn an_oversize_length_prefix_is_rejected_without_reading_the_payload() {
    let mut frame = ((MAX_MESSAGE_BYTES as u32) + 1).to_be_bytes().to_vec();
    frame.extend_from_slice(b"payload that must never be read");
    let mut reader = ChunkedReader::new(&frame, 1);

    let error =
        read_message::<ClientMessage>(&mut reader).expect_err("an oversize frame is refused");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    // The refusal happens on the length prefix alone: the four prefix bytes are
    // consumed and nothing else is, so the allocation never happens either.
    assert_eq!(reader.bytes.len(), frame.len() - 4);
}

/// xorshift64*, so a failing case is reproducible from the printed seed with no
/// dependency on a property-testing crate.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 33) as u8
    }
}

#[test]
fn malformed_postcard_bodies_always_error_and_never_panic() {
    let valid = encode(&sample_message(1));
    let payload = &valid[4..];

    for seed in 1..=256_u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut corrupted = payload.to_vec();
        // Three mutation shapes, because each breaks postcard somewhere else:
        // a flipped discriminant, a truncated varint, and a grown body whose
        // declared lengths now overrun it.
        match seed % 3 {
            0 => {
                let index = rng.below(corrupted.len());
                corrupted[index] ^= 1 << rng.below(8);
            }
            1 => {
                let keep = rng.below(corrupted.len());
                corrupted.truncate(keep);
            }
            _ => {
                let extra = 1 + rng.below(16);
                for _ in 0..extra {
                    corrupted.push(rng.byte());
                }
            }
        }
        if corrupted == payload {
            continue;
        }

        let mut frame = (corrupted.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&corrupted);
        // The frame is fed one byte at a time so a decoder that over-reads its
        // payload would hit EOF on the *reader*, not on the buffer.
        let mut reader = ChunkedReader::new(&frame, 1);

        // Not `expect_err`: a corrupted body is allowed to decode to some other
        // valid message. What is never allowed is a panic, and postcard is
        // reached only through `take_from_bytes`, so an over-long body is a
        // trailing-bytes error rather than a silent truncation.
        if let Err(error) = read_message::<ClientMessage>(&mut reader) {
            assert_eq!(
                error.kind(),
                io::ErrorKind::InvalidData,
                "seed {seed} produced an unexpected error kind: {error}"
            );
        }
    }
}

#[test]
fn a_message_round_trips_through_a_length_prefixed_frame() {
    let message = sample_message(7);
    let frame = encode(&message);

    assert_eq!(
        u32::from_be_bytes(frame[..4].try_into().expect("length prefix")) as usize,
        frame.len() - 4,
        "the prefix must describe exactly the payload that follows"
    );
    let decoded: ClientMessage = read_message(&mut frame.as_slice()).expect("decode round trip");
    assert_eq!(decoded, message);
}
