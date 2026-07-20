use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 8;
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

    // No XDG_RUNTIME_DIR: fall back to a per-user directory under /tmp. Key it
    // on the real effective UID, never on $USER/$UID — those are spoofable and
    // can collide, letting another local user predict or pre-create ("squat")
    // the path. `ensure_private_dir` verifies ownership before the socket binds.
    PathBuf::from("/tmp")
        .join(format!("mult-{}", socket_user_component()))
        .join(DEFAULT_SOCKET_NAME)
}

/// Stable, unspoofable per-user component for the `/tmp` socket fallback.
fn socket_user_component() -> String {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` is always successful and has no failure mode.
        unsafe { libc::geteuid() }.to_string()
    }

    #[cfg(not(unix))]
    {
        sanitize_socket_path_component(
            &env::var("USERNAME")
                .or_else(|_| env::var("USER"))
                .unwrap_or_else(|_| "unknown".to_string()),
        )
    }
}

#[cfg(not(unix))]
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

/// Create `dir` (and any missing parents) as a private directory, then verify
/// it cannot have been pre-created by another local user.
///
/// The directory is made with mode `0700`. Because the `/tmp` fallback lives
/// under a world-writable, sticky parent, a plain `mkdir -p` that tolerates an
/// existing path would happily adopt a directory an attacker created first. So
/// after creating, every component we own is checked — via `lstat`, so symlinks
/// are never followed — to be a real directory owned by us with no group/other
/// write bit. The walk stops at a shared system root (a sticky, world-writable
/// directory such as `/tmp`) or a root-owned ancestor; a component owned by some
/// other non-root user is rejected as a squatter.
#[cfg(unix)]
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;

    // SAFETY: `geteuid` is always successful and has no failure mode.
    let euid = unsafe { libc::geteuid() } as u32;
    let mut cursor = Some(dir);
    while let Some(path) = cursor {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() {
            return Err(private_dir_error(path, "is not a directory"));
        }

        let mode = metadata.mode();
        // A sticky, world-writable directory (e.g. /tmp, /dev/shm) is a shared
        // system root; the private subtree below it is what must be ours. Stop
        // at the filesystem root as well: sandbox roots can be mapped to a
        // synthetic UID even though every mutable descendant was checked.
        if (mode & 0o1000 != 0 && mode & 0o002 != 0) || path.parent().is_none() {
            break;
        }

        let owner = metadata.uid();
        if owner == euid {
            if mode & 0o022 != 0 {
                return Err(private_dir_error(path, "is writable by group or others"));
            }
        } else if owner != 0 {
            return Err(private_dir_error(
                path,
                "is owned by another user (refusing a pre-created path)",
            ));
        }

        cursor = path.parent();
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn private_dir_error(path: &Path, problem: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "refusing to use runtime directory {}: it {problem}",
            path.display()
        ),
    )
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
pub struct ForegroundProcessInfo {
    pub root_pid: Option<u32>,
    pub foreground_pid: Option<u32>,
    pub command: Option<String>,
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
    ForegroundProcess {
        pane: PaneId,
        process: ForegroundProcessInfo,
    },
    PaneExited {
        pane: PaneId,
        exit: ExitInfo,
    },
    StopResult {
        pane: PaneId,
        stopped: bool,
        error: Option<String>,
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
    let (message, trailing) = postcard::take_from_bytes(&payload).map_err(invalid_data)?;
    if !trailing.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message contains trailing bytes",
        ));
    }
    Ok(message)
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let payload = postcard::to_allocvec(message).map_err(invalid_data)?;
    if payload.len() > MAX_MESSAGE_BYTES {
        return Err(message_too_large(
            io::ErrorKind::InvalidInput,
            payload.len() as u64,
        ));
    }

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

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
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
    fn round_trips_correlated_stop_result() {
        let message = ServerMessage::StopResult {
            pane: PaneId(7),
            stopped: false,
            error: Some("kill failed".to_string()),
        };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).expect("write message");
        let decoded: ServerMessage = read_message(&mut bytes.as_slice()).expect("read message");

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

    #[cfg(not(unix))]
    #[test]
    fn socket_path_component_sanitization_removes_path_separators() {
        assert_eq!(sanitize_socket_path_component("user-1000"), "user-1000");
        assert_eq!(sanitize_socket_path_component("../bad/user"), "___bad_user");
        assert_eq!(sanitize_socket_path_component(""), "unknown");
    }

    #[cfg(unix)]
    #[test]
    fn socket_user_component_is_the_numeric_effective_uid() {
        // The /tmp fallback must be keyed off geteuid(), not spoofable env, and
        // must contain no path separators or other surprises.
        let component = socket_user_component();
        assert!(!component.is_empty());
        assert!(component.chars().all(|ch| ch.is_ascii_digit()));
        assert_eq!(component, unsafe { libc::geteuid() }.to_string());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_creates_and_accepts_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_test_dir("create").join("nested");
        ensure_private_dir(&dir).expect("create private dir");
        assert!(dir.is_dir());
        let mode = fs::metadata(&dir).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Idempotent: a second call on the directory we own succeeds.
        ensure_private_dir(&dir).expect("re-accept private dir");

        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_rejects_group_or_other_writable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_test_dir("loose");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).expect("chmod");

        let error = ensure_private_dir(&dir).expect_err("world-writable dir must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_rejects_a_symlinked_directory() {
        let base = unique_test_dir("symlink");
        let target = base.join("real");
        fs::create_dir(&target).expect("create target");
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let error = ensure_private_dir(&link).expect_err("symlinked dir must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    fn unique_test_dir(label: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = env::temp_dir().join(format!(
            "mult-protocol-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("create unique test dir");
        dir
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
    fn read_message_rejects_trailing_bytes_inside_frame() {
        let message = ClientMessage::Attach {
            session: SessionId(1),
            rows: 24,
            cols: 80,
        };
        let mut payload = postcard::to_allocvec(&message).expect("serialize payload");
        payload.push(0xff);
        let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
        bytes.extend(payload);

        let error = read_message::<ClientMessage>(&mut bytes.as_slice())
            .expect_err("trailing bytes should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("trailing bytes"));
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
