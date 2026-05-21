use std::{
    collections::BTreeMap,
    env,
    io::{self, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 4;
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
pub struct ScreenSnapshot {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<TerminalCell>,
    pub cursor: Option<Cursor>,
    pub scrollback_rows: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenUpdate {
    pub rows: u16,
    pub cols: u16,
    pub changed_rows: Vec<RowUpdate>,
    pub cursor: Option<Cursor>,
    pub scrollback_rows: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowUpdate {
    pub row: u16,
    pub cells: Vec<TerminalCell>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCell {
    pub ch: char,
    pub style: TerminalCellStyle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCellStyle {
    pub fg: Option<TerminalColor>,
    pub bg: Option<TerminalColor>,
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
    pub reversed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRenderLine {
    pub spans: Vec<TerminalRenderSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRenderSpan {
    pub text: String,
    pub style: TerminalCellStyle,
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
    Snapshot {
        pane: PaneId,
        snapshot: ScreenSnapshot,
    },
    Update {
        pane: PaneId,
        update: ScreenUpdate,
    },
    PaneExited {
        pane: PaneId,
        exit: ExitInfo,
    },
    Error {
        message: String,
    },
}

impl ScreenSnapshot {
    pub fn blank(rows: u16, cols: u16) -> Self {
        let (rows, cols) = bounded_screen_dimensions(rows, cols);
        Self {
            rows,
            cols,
            cells: vec![TerminalCell::blank(); screen_cell_count(rows, cols)],
            cursor: None,
            scrollback_rows: 0,
        }
    }

    pub fn is_blank(&self) -> bool {
        self.cells.iter().all(|cell| cell.ch == ' ')
    }

    pub fn apply_update(&mut self, update: ScreenUpdate) {
        let (rows, cols) = bounded_screen_dimensions(update.rows, update.cols);
        if self.rows != rows
            || self.cols != cols
            || self.cells.len() != screen_cell_count(rows, cols)
        {
            *self = Self::blank(rows, cols);
        }

        self.cursor = bounded_cursor(update.cursor, rows, cols);
        self.scrollback_rows = update.scrollback_rows;

        let cols = usize::from(self.cols);
        for row in update.changed_rows {
            let row_index = usize::from(row.row);
            if row_index >= usize::from(self.rows) {
                continue;
            }
            let start = row_index.saturating_mul(cols);
            let end = start.saturating_add(cols).min(self.cells.len());
            let target = &mut self.cells[start..end];
            target.fill(TerminalCell::blank());
            let copy_len = target.len().min(row.cells.len());
            target[..copy_len].copy_from_slice(&row.cells[..copy_len]);
        }
    }

    pub fn render_lines(&self) -> Vec<TerminalRenderLine> {
        let (rows, cols) = bounded_screen_dimensions(self.rows, self.cols);
        let rows = usize::from(rows);
        let cols = usize::from(cols);
        (0..rows)
            .map(|row| {
                let start = row.saturating_mul(cols);
                let end = start.saturating_add(cols).min(self.cells.len());
                let cells = if start < self.cells.len() {
                    &self.cells[start..end]
                } else {
                    &[]
                };
                let cursor_col = self.cursor.and_then(|cursor| {
                    (cursor.visible && usize::from(cursor.row) == row)
                        .then_some(usize::from(cursor.col).min(cols.saturating_sub(1)))
                });
                render_terminal_row(cells, cursor_col)
            })
            .collect()
    }
}

impl ScreenUpdate {
    pub fn from_snapshot(snapshot: &ScreenSnapshot) -> Self {
        let (rows, cols) = bounded_screen_dimensions(snapshot.rows, snapshot.cols);
        Self {
            rows,
            cols,
            changed_rows: (0..rows)
                .map(|row| RowUpdate {
                    row,
                    cells: snapshot_row(snapshot, usize::from(row)),
                })
                .collect(),
            cursor: bounded_cursor(snapshot.cursor, rows, cols),
            scrollback_rows: snapshot.scrollback_rows,
        }
    }

    pub fn diff(previous: &ScreenSnapshot, current: &ScreenSnapshot) -> Self {
        let previous_size = bounded_screen_dimensions(previous.rows, previous.cols);
        let current_size = bounded_screen_dimensions(current.rows, current.cols);
        if previous_size != current_size {
            return Self::from_snapshot(current);
        }

        let (rows, cols) = current_size;
        let changed_rows = (0..rows)
            .filter_map(|row| {
                let row_index = usize::from(row);
                let previous_row = snapshot_row(previous, row_index);
                let current_row = snapshot_row(current, row_index);
                (previous_row != current_row).then_some(RowUpdate {
                    row,
                    cells: current_row,
                })
            })
            .collect();

        Self {
            rows,
            cols,
            changed_rows,
            cursor: bounded_cursor(current.cursor, rows, cols),
            scrollback_rows: current.scrollback_rows,
        }
    }
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self::blank()
    }
}

impl TerminalCell {
    pub fn blank() -> Self {
        Self {
            ch: ' ',
            style: TerminalCellStyle::default(),
        }
    }
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

fn screen_cell_count(rows: u16, cols: u16) -> usize {
    usize::from(rows).saturating_mul(usize::from(cols))
}

fn bounded_cursor(cursor: Option<Cursor>, rows: u16, cols: u16) -> Option<Cursor> {
    cursor.map(|cursor| Cursor {
        row: cursor.row.min(rows.saturating_sub(1)),
        col: cursor.col.min(cols.saturating_sub(1)),
        visible: cursor.visible && rows > 0 && cols > 0,
    })
}

fn snapshot_row(snapshot: &ScreenSnapshot, row: usize) -> Vec<TerminalCell> {
    let (_, cols) = bounded_screen_dimensions(snapshot.rows, snapshot.cols);
    let cols = usize::from(cols);
    let start = row.saturating_mul(cols);
    let mut cells = vec![TerminalCell::blank(); cols];
    if start >= snapshot.cells.len() {
        return cells;
    }

    let available = snapshot.cells.len().saturating_sub(start).min(cols);
    cells[..available].copy_from_slice(&snapshot.cells[start..start + available]);
    cells
}

fn render_terminal_row(row: &[TerminalCell], cursor_col: Option<usize>) -> TerminalRenderLine {
    if row.is_empty() {
        return TerminalRenderLine { spans: Vec::new() };
    }
    let last_visible_cell = row
        .iter()
        .rposition(|cell| cell.ch != ' ' || cell.style != TerminalCellStyle::default());
    let last_visible = last_visible_cell.into_iter().chain(cursor_col).max();
    let Some(last_visible) = last_visible.filter(|last_visible| *last_visible < row.len()) else {
        return TerminalRenderLine { spans: Vec::new() };
    };

    let mut spans = Vec::new();
    let mut current_style = row[0].style;
    let mut text = String::new();
    for (index, cell) in row[..=last_visible].iter().enumerate() {
        let mut cell = *cell;
        if cursor_col == Some(index) {
            cell.style = cursor_style(cell.style);
            if cell.ch == ' ' {
                cell.ch = '▌';
            }
        }
        if cell.style != current_style && !text.is_empty() {
            spans.push(TerminalRenderSpan {
                text: std::mem::take(&mut text),
                style: current_style,
            });
        }
        current_style = cell.style;
        text.push(cell.ch);
    }
    if !text.is_empty() {
        spans.push(TerminalRenderSpan {
            text,
            style: current_style,
        });
    }

    TerminalRenderLine { spans }
}

fn cursor_style(mut style: TerminalCellStyle) -> TerminalCellStyle {
    style.fg = Some(TerminalColor::Black);
    style.bg = Some(TerminalColor::BrightWhite);
    style.underlined = false;
    style.reversed = false;
    style
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
    fn snapshot_render_lines_show_cursor() {
        let snapshot = ScreenSnapshot {
            rows: 1,
            cols: 4,
            cells: vec![TerminalCell::blank(); 4],
            cursor: Some(Cursor {
                row: 0,
                col: 0,
                visible: true,
            }),
            scrollback_rows: 0,
        };

        assert_eq!(snapshot.render_lines()[0].spans[0].text, "▌");
    }

    #[test]
    fn blank_snapshot_clamps_huge_dimensions() {
        let snapshot = ScreenSnapshot::blank(u16::MAX, u16::MAX);

        assert!(snapshot.rows <= MAX_SCREEN_ROWS);
        assert!(snapshot.cols <= MAX_SCREEN_COLS);
        assert!(snapshot.cells.len() <= MAX_SCREEN_CELLS);
        assert_eq!(
            snapshot.cells.len(),
            usize::from(snapshot.rows) * usize::from(snapshot.cols)
        );
    }

    #[test]
    fn render_lines_clamps_malformed_snapshot_dimensions() {
        let snapshot = ScreenSnapshot {
            rows: u16::MAX,
            cols: u16::MAX,
            cells: Vec::new(),
            cursor: Some(Cursor {
                row: u16::MAX,
                col: u16::MAX,
                visible: true,
            }),
            scrollback_rows: 0,
        };

        let lines = snapshot.render_lines();

        assert!(lines.len() <= usize::from(MAX_SCREEN_ROWS));
        assert!(lines.len() <= MAX_SCREEN_CELLS / usize::from(MAX_SCREEN_COLS));
    }

    #[test]
    fn applying_huge_screen_update_is_clamped() {
        let mut snapshot = ScreenSnapshot::blank(1, 1);

        snapshot.apply_update(ScreenUpdate {
            rows: u16::MAX,
            cols: u16::MAX,
            changed_rows: Vec::new(),
            cursor: Some(Cursor {
                row: u16::MAX,
                col: u16::MAX,
                visible: true,
            }),
            scrollback_rows: 0,
        });

        assert!(snapshot.rows <= MAX_SCREEN_ROWS);
        assert!(snapshot.cols <= MAX_SCREEN_COLS);
        assert!(snapshot.cells.len() <= MAX_SCREEN_CELLS);
        assert_eq!(
            snapshot.cursor,
            Some(Cursor {
                row: snapshot.rows - 1,
                col: snapshot.cols - 1,
                visible: true,
            })
        );
    }

    #[test]
    fn screen_update_diff_only_includes_changed_rows() {
        let mut previous = ScreenSnapshot::blank(2, 3);
        let mut current = previous.clone();
        current.cells[4].ch = 'x';
        current.cursor = Some(Cursor {
            row: 1,
            col: 1,
            visible: true,
        });

        let update = ScreenUpdate::diff(&previous, &current);
        assert_eq!(update.changed_rows.len(), 1);
        assert_eq!(update.changed_rows[0].row, 1);

        previous.apply_update(update);
        assert_eq!(previous, current);
    }
}
