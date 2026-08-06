//! The main pane: whichever chat or terminal is selected, plus the hints shown
//! when there is nothing to draw yet.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};

use crate::{
    app::{App, FocusMode, NavItem, SearchScope},
    config::{self},
    model::{ChatStatus, PtyKey, TerminalLaunch, WorkspaceId},
    pty::PtyRuntime,
};

use super::terminal_view::render_terminal_parser;
use super::theme::Palette;

#[derive(Debug, Clone, Copy)]
struct PaneRenderStyle {
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

fn render_lines_pane(
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

fn main_pane_focus(item: NavItem) -> FocusMode {
    match item {
        NavItem::Chat { .. } => FocusMode::Chat,
        NavItem::Terminal { .. } => FocusMode::Terminal,
    }
}

pub(super) fn focus_is_active(app: &App, focus: FocusMode) -> bool {
    app.focus() == Some(focus)
}

pub(super) fn pane_style(focused: bool, palette: Palette) -> Style {
    if focused {
        Style::default().fg(palette.text).bg(palette.base)
    } else {
        Style::default().fg(palette.text).bg(palette.nc)
    }
}

fn draw_chat_details(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    workspace_id: WorkspaceId,
    chat_id: crate::model::ChatId,
    area: Rect,
    render_style: PaneRenderStyle,
) {
    let PaneRenderStyle { focused, palette } = render_style;
    let output_rows = usize::from(area.height.max(1));
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

    if let Some(lines) = app.filtered_chat_lines(chat_id) {
        let query = app
            .active_search_query_for_chat(chat_id)
            .unwrap_or_default();
        let mut result = search_result_lines("chat transcript", query, lines, output_rows, palette);
        // E12: chat search reads the *structured* transcript, which only the
        // experimental process-agent backend writes and which nothing calls
        // today, so it is empty for every chat a user can actually create. A
        // bare "No matches." reads as "your text is not in this chat"; the
        // truth is that there is nothing here to search yet, and the PTY
        // output on screen is not what is being searched.
        if app.chat_transcript_lines(chat_id).is_empty() {
            result.push(Line::from(""));
            result.push(Line::from(Span::styled(
                "This chat has no structured transcript to search.",
                Style::default().fg(palette.gold),
            )));
            result.push(Line::from(Span::styled(
                "Chats run in a PTY; only the experimental process-agent backend records",
                Style::default().fg(palette.muted),
            )));
            result.push(Line::from(Span::styled(
                "searchable messages, and it has no call path yet. Esc clears the search.",
                Style::default().fg(palette.muted),
            )));
        }
        render_lines_pane(frame, area, result, focused, palette, false);
        return;
    }

    let terminal_id = PtyKey::ChatAgent(chat_id);
    if !pty_runtime.terminal_output_is_blank(terminal_id) {
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

    // Named per chat, never hardcoded: a Claude Code chat used to be told that
    // "Pi agent" had not started and to set `pi`'s config keys (F18).
    let agent_name = chat.agent.display_name();
    let (command_key, auto_start_key) = chat.agent.config_keys();
    let lines = if matches!(chat.status, ChatStatus::Thinking | ChatStatus::Waiting) {
        vec![
            Line::from(format!(
                "{agent_name} agent is {}; waiting for output.",
                chat.status.label()
            )),
            Line::from("Type to send input to the selected agent PTY."),
        ]
    } else {
        let mut lines = vec![
            Line::from(format!(
                "{agent_name} agent not started. Type to start it and send input."
            )),
            Line::from(format!("Set `{command_key}`/`{auto_start_key}` in:")),
            Line::from(config::config_path().display().to_string()),
        ];
        let transcript = app.chat_lines(chat_id);
        if !transcript.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from("Saved transcript"));
            lines.extend(
                transcript
                    .into_iter()
                    .rev()
                    .take(output_rows.saturating_sub(5))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .map(Line::from),
            );
        }
        lines
    };
    render_lines_pane(frame, area, lines, focused, palette, false);
}

fn draw_terminal_details(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    workspace_id: WorkspaceId,
    terminal_id: crate::model::TerminalId,
    area: Rect,
    render_style: PaneRenderStyle,
) {
    let PaneRenderStyle { focused, palette } = render_style;
    let output_rows = usize::from(area.height.max(1));
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

    // Lazy on purpose: scraping the screen builds a `String` per row, and no
    // search is active on the overwhelming majority of frames.
    if let Some(lines) = app.terminal_search_matches(terminal_id, || {
        pty_runtime.terminal_all_lines(PtyKey::Terminal(terminal_id))
    }) {
        let query = app
            .active_search
            .as_ref()
            .filter(|search| search.scope == SearchScope::Terminal(terminal_id))
            .map(|search| search.query.as_str())
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

    if pty_runtime.terminal_output_is_blank(PtyKey::Terminal(terminal_id)) {
        let mut lines = vec![if app.terminal_requires_recovery(terminal_id) {
            Line::from(
                "Command was not restored or auto-started. Type or use Start selected PTY to run it deliberately.",
            )
        // Liveness has exactly one source: the runtime's attachment. The
        // persisted `TerminalStatus` this used to read was a second one, and
        // the two could disagree indefinitely (F16).
        } else if pty_runtime.is_running(PtyKey::Terminal(terminal_id)) {
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

fn search_result_lines(
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

    use crate::model::AgentKind;
    use crate::ui::test_support::*;

    /// F18: the not-started hint used to say "Pi agent" and name
    /// `pi_agent_command`/`auto_start_pi_agent` for *every* chat, so a Claude
    /// Code chat was told the wrong command and the wrong two config keys.
    #[test]
    fn the_not_started_hint_names_the_chats_own_agent_and_config_keys() {
        let hint_for = |agent: AgentKind| {
            let mut app = App::default();
            let workspace = app.project.workspaces[0].id;
            let chat = app
                .project
                .add_chat(workspace, "chat".to_string(), ChatStatus::Idle, agent)
                .expect("identity")
                .expect("chat added");
            app.select_item(NavItem::Chat { workspace, chat });
            draw_text(&app, &PtyRuntime::new_offline(), 100, 30)
        };

        let pi = hint_for(AgentKind::Pi);
        assert!(pi.contains("pi agent not started"), "{pi}");
        assert!(
            pi.contains("`pi_agent_command`/`auto_start_pi_agent`"),
            "{pi}"
        );

        let claude = hint_for(AgentKind::ClaudeCode);
        assert!(claude.contains("Claude Code agent not started"), "{claude}");
        assert!(
            claude.contains("`claude_code_command`/`auto_start_claude_code_agent`"),
            "{claude}"
        );
        assert!(!claude.contains("pi_agent_command"), "{claude}");
    }

    #[test]
    fn empty_workspace_hint_matches_current_ctrl_controls() {
        let mut app = App::default();
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

    #[test]
    fn blank_terminal_hint_mentions_always_on_input() {
        let app = App::default();
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

    /// E12: chat search reads the structured transcript, which nothing writes
    /// today. "No matches." on its own is a lie by omission — it reads as "your
    /// text is not in this chat" rather than "there is nothing here to search".
    #[test]
    fn searching_a_chat_says_the_structured_transcript_is_not_populated() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let chat = app
            .project
            .add_chat(
                workspace,
                "chat".to_string(),
                ChatStatus::Idle,
                AgentKind::Pi,
            )
            .expect("identity")
            .expect("chat added");
        app.select_item(NavItem::Chat { workspace, chat });
        assert!(app.begin_search(), "a selected chat can be searched");
        for ch in "hello".chars() {
            app.push_prompt_char(ch);
        }
        app.submit_search();

        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);
        assert!(
            text.contains("no structured transcript to search"),
            "{text}"
        );
        assert!(
            text.contains("experimental process-agent backend"),
            "{text}"
        );
    }
}
