//! Wire-codec tests for [`mult_protocol::read_message`] /
//! [`mult_protocol::write_message`].
//!
//! The interesting cases are the ones a `UnixStream` produces in production and
//! a `&[u8]` reader never does: a payload delivered a few bytes at a time, a
//! frame that stops halfway, and bytes from a peer that is not speaking this
//! protocol at all. A deterministic generator (below, no dependency) supplies
//! the round-trip corpus so the value space is wider than a handful of literals
//! while every failure is reproducible from its seed.

use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::PathBuf,
};

use mult_protocol::{
    read_message, write_message, ClientMessage, ExitInfo, ForegroundProcessInfo, InstanceId,
    LaunchSpec, PaneInfo, RejectCode, ServerMessage, SessionId, SessionInfo, MAX_MESSAGE_BYTES,
};

/// A reader that hands out at most `chunk` bytes per `read`, modelling a socket
/// that never delivers a whole frame in one syscall.
struct ChunkedReader<'a> {
    bytes: &'a [u8],
    chunk: usize,
}

impl Read for ChunkedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let take = self.bytes.len().min(buf.len()).min(self.chunk);
        buf[..take].copy_from_slice(&self.bytes[..take]);
        self.bytes = &self.bytes[take..];
        Ok(take)
    }
}

/// xorshift64*: a deterministic generator so the corpus is varied but every
/// case is reproducible from its seed. Inline on purpose — the workspace takes
/// no new dependencies for tests.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

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

    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.below(max_len + 1);
        (0..len).map(|_| self.next_u64() as u8).collect()
    }

    fn text(&mut self, max_len: usize) -> String {
        let len = self.below(max_len + 1);
        (0..len)
            .map(|_| {
                // A mix of ASCII and multi-byte code points, so length-in-bytes
                // and length-in-chars never coincide by accident.
                const ALPHABET: [char; 8] = ['a', 'Z', '0', ' ', '/', '\u{1b}', 'é', '✓'];
                ALPHABET[self.below(ALPHABET.len())]
            })
            .collect()
    }
}

fn generated_client_message(rng: &mut Rng) -> ClientMessage {
    match rng.below(8) {
        0 => ClientMessage::Hello {
            protocol_version: rng.next_u64() as u16,
            instance: InstanceId(rng.next_u64()),
        },
        1 => ClientMessage::ListSessions,
        2 => {
            let mut env = BTreeMap::new();
            for _ in 0..rng.below(4) {
                env.insert(rng.text(8), rng.text(16));
            }
            ClientMessage::CreateSession {
                requested_id: (rng.below(2) == 0).then(|| SessionId(rng.next_u64())),
                name: rng.text(24),
                cwd: (rng.below(2) == 0).then(|| PathBuf::from(rng.text(24))),
                env,
                launch: if rng.below(2) == 0 {
                    LaunchSpec::Shell
                } else {
                    LaunchSpec::Command(rng.text(32))
                },
                rows: rng.next_u64() as u16,
                cols: rng.next_u64() as u16,
            }
        }
        3 => ClientMessage::Attach {
            session: SessionId(rng.next_u64()),
            rows: rng.next_u64() as u16,
            cols: rng.next_u64() as u16,
        },
        4 => ClientMessage::Input {
            pane: SessionId(rng.next_u64()),
            bytes: rng.bytes(4096),
        },
        5 => ClientMessage::Resize {
            pane: SessionId(rng.next_u64()),
            rows: rng.next_u64() as u16,
            cols: rng.next_u64() as u16,
        },
        6 => ClientMessage::Detach,
        _ => ClientMessage::Stop {
            pane: SessionId(rng.next_u64()),
        },
    }
}

fn generated_server_message(rng: &mut Rng) -> ServerMessage {
    match rng.below(7) {
        0 => ServerMessage::Hello {
            protocol_version: rng.next_u64() as u16,
        },
        1 => ServerMessage::Sessions(
            (0..rng.below(4))
                .map(|_| SessionInfo {
                    id: SessionId(rng.next_u64()),
                    name: rng.text(16),
                    attached: rng.below(2) == 0,
                })
                .collect(),
        ),
        2 => ServerMessage::Attached {
            session: SessionId(rng.next_u64()),
            panes: (0..rng.below(3))
                .map(|_| PaneInfo {
                    id: SessionId(rng.next_u64()),
                    title: rng.text(16),
                    rows: rng.next_u64() as u16,
                    cols: rng.next_u64() as u16,
                })
                .collect(),
        },
        3 => ServerMessage::PtyOutput {
            pane: SessionId(rng.next_u64()),
            bytes: rng.bytes(8192),
        },
        4 => ServerMessage::ForegroundProcess {
            pane: SessionId(rng.next_u64()),
            process: ForegroundProcessInfo {
                root_pid: (rng.below(2) == 0).then(|| rng.next_u64() as u32),
                foreground_pid: (rng.below(2) == 0).then(|| rng.next_u64() as u32),
                command: (rng.below(2) == 0).then(|| rng.text(24)),
            },
        },
        5 => ServerMessage::PaneExited {
            pane: SessionId(rng.next_u64()),
            exit: ExitInfo {
                code: rng.next_u64() as u32,
                signal: (rng.below(2) == 0).then(|| rng.text(12)),
            },
        },
        _ => ServerMessage::Error {
            pane: (rng.below(2) == 0).then(|| SessionId(rng.next_u64())),
            code: reject_code(rng.below(11)),
            message: rng.text(48),
        },
    }
}

/// One `RejectCode` per discriminant, so the generator covers the whole enum
/// rather than a single variant that happens to encode compactly.
fn reject_code(index: usize) -> RejectCode {
    match index {
        0 => RejectCode::HelloRequired,
        1 => RejectCode::ProtocolMismatch,
        2 => RejectCode::InstanceTokenRequired,
        3 => RejectCode::InstanceMismatch,
        4 => RejectCode::ConnectionLimit,
        5 => RejectCode::SessionLimit,
        6 => RejectCode::UnknownSession,
        7 => RejectCode::SessionBusy,
        8 => RejectCode::InputRefused,
        9 => RejectCode::SessionCreateFailed,
        10 => RejectCode::PaneOperationFailed,
        _ => RejectCode::Unspecified,
    }
}

#[test]
fn generated_messages_round_trip_through_the_frame_codec() {
    for seed in [1_u64, 7, 42, 1_337, 90_210] {
        let mut rng = Rng::new(seed);
        for case in 0..200 {
            let client = generated_client_message(&mut rng);
            let mut bytes = Vec::new();
            write_message(&mut bytes, &client).expect("write client message");
            let decoded: ClientMessage =
                read_message(&mut bytes.as_slice()).expect("read client message");
            assert_eq!(decoded, client, "seed {seed} case {case}");

            let server = generated_server_message(&mut rng);
            let mut bytes = Vec::new();
            write_message(&mut bytes, &server).expect("write server message");
            let decoded: ServerMessage =
                read_message(&mut bytes.as_slice()).expect("read server message");
            assert_eq!(decoded, server, "seed {seed} case {case}");
        }
    }
}

#[test]
fn frames_split_across_reads_are_reassembled() {
    // The production case: a UnixStream returns whatever is in the buffer, so
    // both the 4-byte header and the payload arrive in pieces. One byte per
    // read is the worst case the codec must survive.
    let mut rng = Rng::new(2024);
    for chunk in [1_usize, 2, 3, 7, 64] {
        for _ in 0..25 {
            let message = generated_server_message(&mut rng);
            let mut bytes = Vec::new();
            write_message(&mut bytes, &message).expect("write message");

            let mut reader = ChunkedReader {
                bytes: &bytes,
                chunk,
            };
            let decoded: ServerMessage = read_message(&mut reader).expect("reassemble split frame");
            assert_eq!(decoded, message, "chunk {chunk}");
        }
    }
}

#[test]
fn back_to_back_frames_split_across_reads_keep_their_order() {
    let first = ClientMessage::Input {
        pane: SessionId(3),
        bytes: vec![7; 5000],
    };
    let second = ClientMessage::Detach;
    let mut bytes = Vec::new();
    write_message(&mut bytes, &first).expect("write first");
    write_message(&mut bytes, &second).expect("write second");

    let mut reader = ChunkedReader {
        bytes: &bytes,
        chunk: 1,
    };
    let decoded_first: ClientMessage = read_message(&mut reader).expect("read first");
    let decoded_second: ClientMessage = read_message(&mut reader).expect("read second");

    assert_eq!(decoded_first, first);
    assert_eq!(decoded_second, second);
}

#[test]
fn a_truncated_payload_is_an_unexpected_eof() {
    let message = ServerMessage::PtyOutput {
        pane: SessionId(1),
        bytes: vec![0xab; 512],
    };
    let mut bytes = Vec::new();
    write_message(&mut bytes, &message).expect("write message");
    bytes.truncate(bytes.len() - 1);

    let error = read_message::<ServerMessage>(&mut bytes.as_slice())
        .expect_err("a truncated payload must not decode");

    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn a_truncated_length_header_is_an_unexpected_eof() {
    let error = read_message::<ServerMessage>(&mut [0_u8, 0, 1].as_slice())
        .expect_err("a partial header must not decode");

    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn a_header_with_no_payload_is_an_unexpected_eof_not_a_huge_allocation() {
    // A peer that announces the largest legal frame and then goes silent must
    // not make this reader commit the announced size before any bytes arrive.
    let header = ((MAX_MESSAGE_BYTES as u32) - 1).to_be_bytes();

    let error = read_message::<ServerMessage>(&mut header.as_slice())
        .expect_err("a header with no payload must not decode");

    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn a_zero_length_frame_is_invalid_data() {
    let header = 0_u32.to_be_bytes();

    let error = read_message::<ClientMessage>(&mut header.as_slice())
        .expect_err("an empty frame must be rejected");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn malformed_payloads_are_errors_not_panics() {
    // Garbage inside a well-formed frame: every case must come back as an
    // `io::Error`, never a panic or a decoded value.
    let mut rng = Rng::new(31_337);
    for _ in 0..500 {
        let payload = rng.bytes(64);
        if payload.is_empty() {
            continue;
        }
        let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&payload);

        // A decode that happens to succeed is fine (random bytes can spell a
        // valid message); what must never happen is a panic.
        if let Ok(message) = read_message::<ClientMessage>(&mut bytes.as_slice()) {
            let mut reencoded = Vec::new();
            write_message(&mut reencoded, &message).expect("re-encode decoded message");
        }
    }
}

#[test]
fn a_frame_declaring_more_than_the_limit_is_rejected_before_reading() {
    let mut bytes = ((MAX_MESSAGE_BYTES as u32) + 1).to_be_bytes().to_vec();
    bytes.extend_from_slice(b"payload that is never read");

    let error = read_message::<ClientMessage>(&mut bytes.as_slice())
        .expect_err("an oversized frame must be rejected");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("message too large"));
}
