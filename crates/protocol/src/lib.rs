use std::{
    collections::BTreeMap,
    env,
    io::{self, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 5;
pub const DEFAULT_SOCKET_NAME: &str = "mult.sock";
pub const SOCKET_PATH_ENV: &str = "MULT_SOCKET_PATH";
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SCREEN_ROWS: u16 = 1_000;
pub const MAX_SCREEN_COLS: u16 = 1_000;
pub const MAX_SCREEN_CELLS: usize = 200_000;

pub fn default_socket_path() -> PathBuf {
    if let Some(path) = env::var_os(SOCKET_PATH_ENV) {
        return PathBuf::from(path);
    }

    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join(DEFAULT_SOCKET_NAME);
    }

    let user = env::var("UID")
        .or_else(|_| env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from("/tmp").join(format!(
        "mult-{}.sock",
        sanitize_socket_path_component(&user)
    ))
}

fn sanitize_socket_path_component(input: &str) -> String {
    let sanitized = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PaneId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchSpec {
    Shell,
    Command(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub pane: PaneId,
    pub attached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: PaneId,
    pub title: String,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: u32,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello {
        protocol_version: u16,
    },
    ListSessions,
    CreateSession {
        requested_id: Option<SessionId>,
        name: String,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
        launch: LaunchSpec,
        rows: u16,
        cols: u16,
    },
    Attach {
        session: SessionId,
        rows: u16,
        cols: u16,
    },
    Input {
        pane: PaneId,
        bytes: Vec<u8>,
    },
    Paste {
        pane: PaneId,
        text: String,
    },
    Scroll {
        pane: PaneId,
        rows: i32,
    },
    ScrollToTop {
        pane: PaneId,
    },
    ScrollToBottom {
        pane: PaneId,
    },
    Resize {
        pane: PaneId,
        rows: u16,
        cols: u16,
    },
    Detach,
    Stop {
        pane: PaneId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    Hello {
        protocol_version: u16,
    },
    Sessions(Vec<SessionInfo>),
    Attached {
        session: SessionId,
        panes: Vec<PaneInfo>,
    },
    PtyScrollback {
        pane: PaneId,
        bytes: Vec<u8>,
    },
    PtyOutput {
        pane: PaneId,
        bytes: Vec<u8>,
    },
    PaneExited {
        pane: PaneId,
        exit: ExitInfo,
    },
    Error {
        message: String,
    },
}

pub fn read_message<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> io::Result<T> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(message_too_large(io::ErrorKind::InvalidData, len as u64));
    }

    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    bincode::deserialize(&payload).map_err(invalid_data)
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let serialized_len = bincode::serialized_size(message).map_err(invalid_data)?;
    if serialized_len > MAX_MESSAGE_BYTES as u64 {
        return Err(message_too_large(
            io::ErrorKind::InvalidInput,
            serialized_len,
        ));
    }

    let payload = bincode::serialize(message).map_err(invalid_data)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too large"))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub fn bounded_screen_dimensions(rows: u16, cols: u16) -> (u16, u16) {
    let rows = rows.min(MAX_SCREEN_ROWS);
    let cols = cols.min(MAX_SCREEN_COLS);
    if rows == 0 || cols == 0 {
        return (rows, cols);
    }

    let max_rows_by_cells = (MAX_SCREEN_CELLS / usize::from(cols)).max(1);
    (rows.min(max_rows_by_cells as u16), cols)
}

fn message_too_large(kind: io::ErrorKind, len: u64) -> io::Error {
    io::Error::new(
        kind,
        format!("message too large: {len} bytes exceeds {MAX_MESSAGE_BYTES} byte limit"),
    )
}

fn invalid_data(error: bincode::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_messages_with_length_prefix() {
        let message = ClientMessage::Attach {
            session: SessionId(1),
            rows: 24,
            cols: 80,
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).expect("write message");
        let decoded: ClientMessage = read_message(&mut bytes.as_slice()).expect("read message");

        assert_eq!(decoded, message);
    }

    #[test]
    fn round_trips_raw_pty_output() {
        let message = ServerMessage::PtyOutput {
            pane: PaneId(7),
            bytes: b"hello\x1b[31m".to_vec(),
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).expect("write message");
        let decoded: ServerMessage = read_message(&mut bytes.as_slice()).expect("read message");

        assert_eq!(decoded, message);
    }

    #[test]
    fn socket_path_component_sanitization_removes_path_separators() {
        assert_eq!(sanitize_socket_path_component("user-1000"), "user-1000");
        assert_eq!(sanitize_socket_path_component("../bad/user"), "___bad_user");
        assert_eq!(sanitize_socket_path_component(""), "unknown");
    }

    #[test]
    fn socket_path_can_be_overridden_by_environment() {
        let path = PathBuf::from("/tmp/mult-test-override.sock");
        std::env::set_var(SOCKET_PATH_ENV, &path);

        assert_eq!(default_socket_path(), path);

        std::env::remove_var(SOCKET_PATH_ENV);
    }

    #[test]
    fn read_message_rejects_oversized_payload_before_allocating() {
        let bytes = ((MAX_MESSAGE_BYTES as u32) + 1).to_be_bytes();

        let error = read_message::<ClientMessage>(&mut bytes.as_slice())
            .expect_err("oversized message should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn write_message_rejects_oversized_payload() {
        let message = vec![0_u8; MAX_MESSAGE_BYTES + 1];
        let mut bytes = Vec::new();

        let error = write_message(&mut bytes, &message).expect_err("reject payload");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(bytes.is_empty());
    }

    #[test]
    fn bounded_screen_dimensions_clamps_huge_sizes() {
        let (rows, cols) = bounded_screen_dimensions(u16::MAX, u16::MAX);

        assert!(rows <= MAX_SCREEN_ROWS);
        assert!(cols <= MAX_SCREEN_COLS);
        assert!(usize::from(rows) * usize::from(cols) <= MAX_SCREEN_CELLS);
    }
}
