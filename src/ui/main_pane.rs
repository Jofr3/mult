//! The main pane: whichever chat or terminal is selected, its search results,
//! its blank-pane hint, and the text selection drawn over its output.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::{
    app::{App, FocusMode, NavItem, SearchScope, TextSelection},
    config,
    model::{ChatStatus, PtyKey, TerminalLaunch, WorkspaceId},
    pty::{PtyRuntime, MIN_PTY_COLS, MIN_PTY_ROWS},
};

use crate::layout::output_area;

use super::{
    focus_is_active,
    selection::render_text_selection,
    theme::{pane_style, Palette},
    vt_screen::TerminalScreen,
};

#[derive(Debug, Clone, Copy)]
pub(super) struct PaneRenderStyle {
    focused: bool,
    palette: Palette,
}

pub(super) fn draw_main(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    area: Rect,
    palette: Palette,
) {
    let selected_item = app.selected_item();
    let pane_focus = selected_item.map(main_pane_focus);
    let focused = pane_focus.is_some_and(|focus| focus_is_active(app, focus));

    match selected_item {
        Some(NavItem::Chat { workspace, chat }) => {
            draw_chat_details(
                frame,
                app,
                pty_runtime,
                workspace,
                chat,
                area,
                PaneRenderStyle { focused, palette },
            );
        }
        Some(NavItem::Terminal {
            workspace,
            terminal,
        }) => draw_terminal_details(
            frame,
            app,
            pty_runtime,
            workspace,
            terminal,
            area,
            PaneRenderStyle { focused, palette },
        ),
        None => {
            let message = if app.project.workspaces.is_empty() {
                "No workspaces yet. Press Ctrl-f to open one."
            } else {
                "No chats or terminals. Press Ctrl-a or Ctrl-t to add one."
            };
            render_lines_pane(
                frame,
                area,
                vec![Line::from(message)],
                focused,
                palette,
                true,
            );
        }
    }
}

pub(super) fn render_lines_pane(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    focused: bool,
    palette: Palette,
    wrap: bool,
) {
    let paragraph = Paragraph::new(lines)
        .block(Block::default().style(pane_style(focused, palette)))
        .style(pane_style(focused, palette));
    let paragraph = if wrap {
        paragraph.wrap(Wrap { trim: false })
    } else {
        paragraph
    };
    frame.render_widget(paragraph, area);
}

pub(super) fn main_pane_focus(item: NavItem) -> FocusMode {
    match item {
        NavItem::Chat { .. } => FocusMode::Chat,
        NavItem::Terminal { .. } => FocusMode::Terminal,
    }
}

pub(super) fn draw_chat_details(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    workspace_id: WorkspaceId,
    chat_id: crate::model::ChatId,
    area: Rect,
    render_style: PaneRenderStyle,
) {
    let PaneRenderStyle { focused, palette } = render_style;
    let output_rows = usize::from(output_area(area).height.max(1));
    if app.project.workspace(workspace_id).is_none() {
        render_lines_pane(
            frame,
            area,
            vec![Line::from("Missing workspace.")],
            focused,
            palette,
            false,
        );
        return;
    }
    let Some(chat) = app.project.chat(workspace_id, chat_id) else {
        render_lines_pane(
            frame,
            area,
            vec![Line::from("Missing chat.")],
            focused,
            palette,
            false,
        );
        return;
    };

    let terminal_id = PtyKey::ChatAgent(chat_id);

    // Chat search runs over the agent PTY's screen, which is where a chat's
    // content actually lives. It used to filter a persisted transcript that no
    // production code path ever wrote (F1).
    if let Some(lines) = app.search_matches(SearchScope::Chat(chat_id), || {
        pty_runtime.pty_lines(terminal_id)
    }) {
        let query = app
            .active_search_query_for(SearchScope::Chat(chat_id))
            .unwrap_or_default();
        render_lines_pane(
            frame,
            area,
            search_result_lines("chat", query, lines, output_rows, palette),
            focused,
            palette,
            false,
        );
        return;
    }

    if !pty_runtime.pty_output_is_blank(terminal_id) {
        if let Some(parser) = pty_runtime.parser(terminal_id) {
            render_terminal_parser(
                frame,
                area,
                parser,
                focused,
                palette,
                app.text_selection_for(terminal_id),
            );
            return;
        }
    }

    // Named per agent kind: a Claude Code chat used to be told that "Pi agent"
    // had not started and to go and edit pi's config keys (F18).
    let agent = chat.agent.display_name();
    let lines = if matches!(chat.status, ChatStatus::Thinking | ChatStatus::Waiting) {
        vec![
            Line::from(format!(
                "{agent} agent is {}; waiting for output.",
                chat.status.label()
            )),
            Line::from("Type to send input to the selected agent PTY."),
        ]
    } else {
        let keys = chat.agent.config_keys();
        vec![
            Line::from(format!(
                "{agent} agent not started. Type to start it and send input."
            )),
            Line::from(format!("Set `{}`/`{}` in:", keys.command, keys.auto_start)),
            Line::from(config::config_path().display().to_string()),
        ]
    };
    render_lines_pane(frame, area, lines, focused, palette, false);
}

pub(super) fn draw_terminal_details(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    workspace_id: WorkspaceId,
    terminal_id: crate::model::TerminalId,
    area: Rect,
    render_style: PaneRenderStyle,
) {
    let PaneRenderStyle { focused, palette } = render_style;
    let output_rows = usize::from(output_area(area).height.max(1));
    if app.project.workspace(workspace_id).is_none() {
        render_lines_pane(
            frame,
            area,
            vec![Line::from("Missing workspace.")],
            focused,
            palette,
            false,
        );
        return;
    }
    let Some(terminal) = app.project.terminal(workspace_id, terminal_id) else {
        render_lines_pane(
            frame,
            area,
            vec![Line::from("Missing terminal.")],
            focused,
            palette,
            false,
        );
        return;
    };

    if let Some(lines) = app.search_matches(SearchScope::Terminal(terminal_id), || {
        pty_runtime.pty_lines(PtyKey::Terminal(terminal_id))
    }) {
        let query = app
            .active_search_query_for(SearchScope::Terminal(terminal_id))
            .unwrap_or_default();
        render_lines_pane(
            frame,
            area,
            search_result_lines("terminal", query, lines, output_rows, palette),
            focused,
            palette,
            false,
        );
        return;
    }

    if pty_runtime.pty_output_is_blank(PtyKey::Terminal(terminal_id)) {
        // Liveness comes from the runtime, never from the persisted state: the
        // two used to be separate answers to the same question, and a missed
        // `mark_terminal_stopped` left this pane claiming a dead terminal was
        // running (F16).
        let mut lines = vec![if pty_runtime.is_running(PtyKey::Terminal(terminal_id)) {
            Line::from("Terminal is running; waiting for output. Type to send PTY input.")
        } else {
            Line::from("Terminal is stopped. Type to start it and send input.")
        }];
        if let TerminalLaunch::Command(command) = &terminal.launch {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Command: ", Style::default().fg(palette.muted)),
                Span::raw(command.clone()),
            ]));
        }
        render_lines_pane(frame, area, lines, focused, palette, false);
        return;
    }

    if let Some(parser) = pty_runtime.parser(PtyKey::Terminal(terminal_id)) {
        render_terminal_parser(
            frame,
            area,
            parser,
            focused,
            palette,
            app.text_selection_for(PtyKey::Terminal(terminal_id)),
        );
    }
}

/// What a pane too small to hold a screen shows instead. Kept short so it
/// survives a narrow pane; a pane narrower than this shows nothing at all,
/// which is the honest rendering of "there is no room".
const PANE_TOO_SMALL_NOTICE: &str = "too small";

pub(super) fn render_terminal_parser(
    frame: &mut Frame,
    area: Rect,
    parser: &vt100::Parser,
    focused: bool,
    palette: Palette,
    selection: Option<&TextSelection>,
) {
    // A PTY is never smaller than the emulator's floor (A13), so an area below
    // it cannot show the screen — only a corner of one. Drawing that corner
    // would be a lie: the cursor, the last line and any wrapped output would all
    // be off-pane with nothing to say so. Say so instead.
    if area.height < MIN_PTY_ROWS || area.width < MIN_PTY_COLS {
        // A pane too narrow even for the notice is left blank rather than shown
        // a truncated word, which would read as content.
        let lines = if usize::from(area.width) >= PANE_TOO_SMALL_NOTICE.chars().count() {
            vec![Line::styled(
                PANE_TOO_SMALL_NOTICE,
                Style::default().fg(palette.muted),
            )]
        } else {
            Vec::new()
        };
        render_lines_pane(frame, area, lines, focused, palette, false);
        return;
    }

    let cursor_style = Style::default().fg(palette.cursor).bg(palette.base);
    // The overlay is what marks the cell the child's cursor is on; under
    // `NO_COLOR` it is a reversed modifier, not a colour pair (F15).
    let cursor_overlay_style = palette.emphasis_style(palette.cursor, palette.nc);
    let cursor = Cursor::default()
        .symbol("█")
        .style(cursor_style)
        .overlay_style(cursor_overlay_style)
        .visibility(parser.screen().scrollback() == 0);
    let terminal_screen = TerminalScreen::from_vt100(parser.screen());
    let pseudo_term = PseudoTerminal::new(&terminal_screen)
        .block(Block::default().style(pane_style(focused, palette)))
        .cursor(cursor);
    frame.render_widget(pseudo_term, area);
    if let Some(selection) = selection {
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let row_content_widths: Vec<u16> = (0..rows)
            .map(|row| {
                (0..cols)
                    .rev()
                    .find(|&col| screen.cell(row, col).is_some_and(vt100::Cell::has_contents))
                    .map_or(0, |col| col.saturating_add(1))
            })
            .collect();
        render_text_selection(frame, area, &row_content_widths, selection, palette);
    }
}
pub(super) fn search_result_lines(
    scope: &'static str,
    query: &str,
    lines: Vec<String>,
    output_rows: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    let mut output = vec![Line::from(vec![
        Span::styled("Search ", Style::default().fg(palette.muted)),
        Span::styled(scope, Style::default().fg(palette.foam)),
        Span::styled(": ", Style::default().fg(palette.muted)),
        Span::styled(query.to_string(), Style::default().fg(palette.gold)),
        Span::styled(
            format!(
                " ({} match{})",
                lines.len(),
                if lines.len() == 1 { "" } else { "es" }
            ),
            Style::default().fg(palette.muted),
        ),
    ])];

    if lines.is_empty() {
        output.push(Line::from("No matches."));
        return output;
    }

    output.extend(
        lines
            .into_iter()
            .rev()
            .take(output_rows.saturating_sub(1))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(Line::from),
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    use crate::layout::AppLayout;

    use super::super::{
        draw,
        test_support::{buffer_text, draw_text, test_palette},
    };

    #[test]
    fn empty_workspace_hint_matches_current_ctrl_controls() {
        let mut app = App::two_workspaces();
        app.project.workspaces.truncate(1);
        app.project.workspaces[0].chats.clear();
        app.project.workspaces[0].terminals.clear();
        app.select_nav_index(0);

        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);

        assert!(text.contains("Press Ctrl-a or Ctrl-t"));
        assert!(!text.contains("Ctrl-c command"));
        assert!(!text.contains("Press `n a`"));
    }
    #[test]
    fn blank_chat_hint_mentions_always_on_input() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.project.workspaces[0].chats[0].status = ChatStatus::Waiting;

        app.select_item(NavItem::Chat { workspace, chat });
        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);

        assert!(text.contains("Type to send input to the selected agent PTY."));
        assert!(!text.contains("input mode"));
    }
    /// F18: the "not started" hint used to name pi's command and pi's config
    /// keys for every agent kind, so a Claude Code chat was pointed at settings
    /// that do not affect it.
    #[test]
    fn the_not_started_hint_names_the_chats_own_agent_and_config_keys() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.project.workspaces[0].chats[0].agent = crate::model::AgentKind::ClaudeCode;
        app.project.workspaces[0].chats[0].status = ChatStatus::Idle;
        app.select_item(NavItem::Chat { workspace, chat });

        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);

        assert!(text.contains("Claude Code agent not started"), "{text}");
        assert!(text.contains("claude_code_command"), "{text}");
        assert!(text.contains("auto_start_claude_code_agent"), "{text}");
        assert!(!text.contains("pi_agent_command"), "{text}");
    }
    #[test]
    fn blank_terminal_hint_mentions_always_on_input() {
        let app = App::two_workspaces();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;

        let mut app = app;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);

        assert!(text.contains("Type to start it and send input."));
        assert!(!text.contains("Press `i`"));
        assert!(!text.contains("input mode"));
    }
    /// A13: the PTY is never below 2×2, so an area below that cannot hold the
    /// screen — only its top-left corner. Drawing the corner would present part
    /// of a screen as though it were all of it, with the cursor and the last
    /// line silently off-pane, so the pane says it does not fit instead.
    #[test]
    fn a_pane_too_small_for_a_screen_says_so_instead_of_drawing_a_corner() {
        let mut pty_runtime = PtyRuntime::new_offline();
        let pty = PtyKey::Terminal(crate::model::TerminalId::new(9).unwrap());
        pty_runtime.reset_parser(pty, crate::pty::PtyDimensions::new(1, 1));
        pty_runtime.process_pty_output(pty, b"abcdefgh");
        let parser = pty_runtime.parser(pty).expect("pane has a screen");

        let render = |width: u16, height: u16| {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("create test terminal");
            terminal
                .draw(|frame| {
                    render_terminal_parser(
                        frame,
                        frame.area(),
                        parser,
                        false,
                        test_palette(),
                        None,
                    );
                })
                .expect("draw pane");
            buffer_text(terminal.backend(), 0, 0, width)
        };

        // One row is below the floor whatever the width, and so is one column.
        assert!(render(20, 1).contains(PANE_TOO_SMALL_NOTICE));
        assert!(render(1, 20).trim().is_empty(), "no room for the notice");
        // Two rows and two columns is the smallest pane that draws a screen.
        assert!(!render(20, 2).contains(PANE_TOO_SMALL_NOTICE));
    }
    #[test]
    fn scrolled_terminal_output_hides_cursor() {
        let mut app = App::two_workspaces();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.reset_parser(terminal_id, crate::pty::PtyDimensions::new(2, 8));
        pty_runtime.process_pty_output(terminal_id, b"one\r\ntwo\r\nthree");
        assert!(pty_runtime.scroll_up(terminal_id, 1));

        let text = draw_text(&app, &pty_runtime, 50, 6);

        assert!(!text.contains('█'));
    }
    #[test]
    fn terminal_cursor_uses_white_on_blank_cell() {
        let mut app = App::two_workspaces();
        let nav_items = app.nav_items();
        let (selected, terminal_id) = nav_items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);

        let frame_area = Rect::new(0, 0, 50, 6);
        let (_, output_area) = AppLayout::compute(&app, frame_area)
            .selected_terminal_output()
            .expect("terminal selection has output area");
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.reset_parser(
            terminal_id,
            crate::pty::PtyDimensions::new(output_area.height, output_area.width),
        );
        pty_runtime.process_pty_output(terminal_id, b"x");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &pty_runtime,
                    &config::Config::default(),
                )
            })
            .expect("draw app");

        let palette = test_palette();
        let cursor_cell = terminal
            .backend()
            .buffer()
            .cell((output_area.x + 1, output_area.y))
            .expect("cursor cell is in bounds");
        assert_eq!(cursor_cell.symbol(), "█");
        assert_eq!(cursor_cell.fg, palette.cursor);
        assert_eq!(cursor_cell.bg, palette.base);
        assert_ne!(cursor_cell.fg, palette.nc);
    }

    #[test]
    fn terminal_output_does_not_wrap_styled_blank_rows() {
        let mut app = App::two_workspaces();
        let nav_items = app.nav_items();
        let (selected, terminal_id) = nav_items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, PtyKey::Terminal(*terminal))),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);

        let frame_area = Rect::new(0, 0, 50, 6);
        let (_, output_area) = AppLayout::compute(&app, frame_area)
            .selected_terminal_output()
            .expect("terminal selection has output area");
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.reset_parser(
            terminal_id,
            crate::pty::PtyDimensions::new(output_area.height, output_area.width),
        );
        let spaces = " ".repeat(usize::from(output_area.width));
        pty_runtime.process_pty_output(
            terminal_id,
            format!("\x1b[44m{spaces}\x1b[0m\r\nnext").as_bytes(),
        );

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &pty_runtime,
                    &config::Config::default(),
                )
            })
            .expect("draw app");

        assert_eq!(
            buffer_text(terminal.backend(), output_area.x, output_area.y + 1, 4),
            "next"
        );
    }
}
