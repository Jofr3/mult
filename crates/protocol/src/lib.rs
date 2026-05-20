use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_SOCKET_NAME: &str = "mult.sock";

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
    Resize {
        pane: PaneId,
        rows: u16,
        cols: u16,
    },
    Detach,
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
        snapshot: ScreenSnapshot,
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
    pub fn is_blank(&self) -> bool {
        self.cells.iter().all(|cell| cell.ch == ' ')
    }

    pub fn render_lines(&self) -> Vec<TerminalRenderLine> {
        let rows = usize::from(self.rows);
        let cols = usize::from(self.cols);
        (0..rows)
            .map(|row| {
                let start = row.saturating_mul(cols);
                let end = start.saturating_add(cols).min(self.cells.len());
                let cursor_col = self.cursor.and_then(|cursor| {
                    (cursor.visible && usize::from(cursor.row) == row)
                        .then_some(usize::from(cursor.col))
                });
                render_terminal_row(&self.cells[start..end], cursor_col)
            })
            .collect()
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
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    bincode::deserialize(&payload).map_err(invalid_data)
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let payload = bincode::serialize(message).map_err(invalid_data)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too large"))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

fn render_terminal_row(row: &[TerminalCell], cursor_col: Option<usize>) -> TerminalRenderLine {
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
    style.fg = Some(TerminalColor::BrightWhite);
    style.bg = None;
    style.underlined = false;
    style
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
}
