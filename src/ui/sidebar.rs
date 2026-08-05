//! The sidebar: one row per workspace header, chat and terminal, in the order
//! `App` produced them.
//!
//! The row order and the nav order are one walk, decided in `App`; this only
//! draws what it is handed and finds the highlight by position (F14).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, HighlightSpacing, List, ListItem, ListState},
    Frame,
};

use crate::{
    app::{App, FocusMode, NavItem, SidebarRow},
    model::{
        ChatSession, ChatStatus, PtyKey, TerminalId, TerminalLaunch, TerminalSession, Workspace,
        WorkspaceId,
    },
    pty::PtyRuntime,
};

use super::{
    focus_is_active,
    text::{text_width, truncate_text},
    theme::{pane_style, Palette},
};

pub(super) const SIDEBAR_SELECTION_SYMBOL: &str = " ";
pub(super) const WORKSPACE_ICON: &str = "▣ ";
pub(super) const GIT_BRANCH_ICON: &str = "";

pub(super) fn draw_sidebar(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    area: Rect,
    palette: Palette,
) {
    let rows = app.sidebar_rows();
    let selected = sidebar_highlight_row(app, &rows);
    let items = sidebar_items(app, pty_runtime, palette, sidebar_item_width(area), &rows);
    let mut state = ListState::default();
    state.select(selected);

    let focused = focus_is_active(app, FocusMode::Sidebar);
    let style = pane_style(focused, palette);
    frame.render_widget(Block::default().style(style), area);

    let list = List::new(items)
        .style(style)
        .highlight_style(palette.selection_style())
        .highlight_symbol(SIDEBAR_SELECTION_SYMBOL)
        .highlight_spacing(HighlightSpacing::Always);

    frame.render_stateful_widget(list, area, &mut state);
}

/// Render the rows `App` produced. The order — and which rows are selectable —
/// is decided there, in one walk; this only draws what it is handed (F14).
pub(super) fn sidebar_items(
    app: &App,
    pty_runtime: &PtyRuntime,
    palette: Palette,
    item_width: usize,
    rows: &[SidebarRow],
) -> Vec<ListItem<'static>> {
    rows.iter()
        .map(|row| match row {
            SidebarRow::Spacer => ListItem::new(Line::from("")),
            SidebarRow::Workspace(workspace) => match app.project.workspace(*workspace) {
                Some(workspace) => ListItem::new(workspace_sidebar_line(
                    workspace,
                    app.workspace_git_branch(workspace.id),
                    palette,
                    item_width,
                )),
                None => ListItem::new(Line::from("")),
            },
            SidebarRow::Nav {
                item: NavItem::Chat { workspace, chat },
                ..
            } => match app.project.chat(*workspace, *chat) {
                Some(chat) => chat_sidebar_item(chat, palette),
                None => ListItem::new(Line::from("")),
            },
            SidebarRow::Nav {
                item:
                    NavItem::Terminal {
                        workspace,
                        terminal,
                    },
                ..
            } => match app.project.terminal(*workspace, *terminal) {
                Some(terminal) => terminal_sidebar_item(
                    app,
                    *workspace,
                    terminal,
                    pty_runtime,
                    palette,
                    item_width,
                ),
                None => ListItem::new(Line::from("")),
            },
        })
        .collect()
}

pub(super) fn chat_sidebar_item(chat: &ChatSession, palette: Palette) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{} ", chat_status_glyph(chat.status)),
            chat_status_style(chat.status, palette),
        ),
        Span::raw(chat_sidebar_label(chat)),
    ]))
}

pub(super) fn terminal_sidebar_item(
    app: &App,
    workspace: WorkspaceId,
    terminal: &TerminalSession,
    pty_runtime: &PtyRuntime,
    palette: Palette,
    item_width: usize,
) -> ListItem<'static> {
    let focused = terminal_sidebar_item_is_focused(app, workspace, terminal.id);
    ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{} ", terminal_glyph(terminal, pty_runtime)),
            terminal_icon_style(terminal, pty_runtime, focused, palette),
        ),
        Span::raw(terminal_display_label(
            terminal,
            pty_runtime,
            item_width.saturating_sub(4),
        )),
    ]))
}

pub(super) fn terminal_sidebar_item_is_focused(
    app: &App,
    workspace: WorkspaceId,
    terminal: TerminalId,
) -> bool {
    focus_is_active(app, FocusMode::Terminal)
        && app.selected_item()
            == Some(NavItem::Terminal {
                workspace,
                terminal,
            })
}

/// The row the highlight sits on: the position of the selected nav item among
/// the rows `App` produced.
///
/// With nothing selected the first selectable row is highlighted, which is what
/// the hand-written walk this replaced did by starting from nav index 0.
pub(super) fn sidebar_highlight_row(app: &App, rows: &[SidebarRow]) -> Option<usize> {
    let target = app.selected_index().unwrap_or(0);
    rows.iter().position(|row| match row {
        SidebarRow::Nav { index, .. } => *index == target,
        SidebarRow::Spacer | SidebarRow::Workspace(_) => false,
    })
}

pub(super) fn sidebar_item_width(area: Rect) -> usize {
    usize::from(
        area.width
            .saturating_sub(text_width(SIDEBAR_SELECTION_SYMBOL) as u16),
    )
}

pub(super) fn workspace_sidebar_line(
    workspace: &Workspace,
    branch: Option<&str>,
    palette: Palette,
    item_width: usize,
) -> Line<'static> {
    if let Some(branch) = branch.filter(|branch| !branch.trim().is_empty()) {
        let workspace_icon_width = text_width(WORKSPACE_ICON);
        let branch_icon_width = text_width(GIT_BRANCH_ICON) + 1;
        let branch_trailing_space_width = 1;
        let minimum_name_width = 1;
        let minimum_branch_name_width = 1;
        let minimum_gap_width = 1;
        let minimum_width = workspace_icon_width
            + minimum_name_width
            + minimum_gap_width
            + branch_icon_width
            + minimum_branch_name_width
            + branch_trailing_space_width;

        if item_width >= minimum_width {
            let max_branch_name_width = item_width.saturating_sub(
                workspace_icon_width
                    + minimum_name_width
                    + minimum_gap_width
                    + branch_icon_width
                    + branch_trailing_space_width,
            );
            let branch_name = truncate_text(branch, max_branch_name_width);
            let branch_width =
                branch_icon_width + text_width(&branch_name) + branch_trailing_space_width;
            let max_name_width =
                item_width.saturating_sub(workspace_icon_width + minimum_gap_width + branch_width);
            let workspace_name = truncate_text(&workspace.name, max_name_width);
            let gap_width = item_width
                .saturating_sub(workspace_icon_width + text_width(&workspace_name) + branch_width);

            return Line::from(vec![
                Span::styled(WORKSPACE_ICON, Style::default().fg(palette.foam)),
                Span::styled(
                    workspace_name,
                    Style::default()
                        .fg(palette.foam)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" ".repeat(gap_width)),
                Span::styled(GIT_BRANCH_ICON, Style::default().fg(palette.iris)),
                Span::raw(" "),
                Span::styled(branch_name, Style::default().fg(palette.iris)),
                Span::raw(" "),
            ]);
        }
    }

    let workspace_icon_width = text_width(WORKSPACE_ICON);
    let workspace_name = truncate_text(
        &workspace.name,
        item_width.saturating_sub(workspace_icon_width),
    );
    Line::from(vec![
        Span::styled(WORKSPACE_ICON, Style::default().fg(palette.foam)),
        Span::styled(
            workspace_name,
            Style::default()
                .fg(palette.foam)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Sidebar label for a chat, tagged with the agent backing it so running
/// agents are distinguishable at a glance, e.g. `agent: pi` or `agent: cc`.
pub(super) fn chat_sidebar_label(chat: &ChatSession) -> String {
    format!("{}: {}", chat.name, chat.agent.label())
}

pub(super) fn terminal_display_label(
    terminal: &TerminalSession,
    pty_runtime: &PtyRuntime,
    max_width: usize,
) -> String {
    truncate_text(&terminal_command_label(terminal, pty_runtime), max_width)
}

pub(super) fn terminal_command_label(
    terminal: &TerminalSession,
    pty_runtime: &PtyRuntime,
) -> String {
    match &terminal.launch {
        TerminalLaunch::Command(command) => command_label_or_default(command),
        TerminalLaunch::Shell => pty_runtime
            .pty_last_command(PtyKey::Terminal(terminal.id))
            .map(command_label_or_default)
            .unwrap_or_else(|| "terminal".to_string()),
    }
}

pub(super) fn command_label_or_default(command: &str) -> String {
    let command = command.trim();
    if command.is_empty() || command == "clear" {
        "terminal".to_string()
    } else {
        command.to_string()
    }
}

/// Sidebar glyph for a chat's status (E8).
///
/// Shape carries the state and colour only reinforces it: every chat used to
/// render the same `●`, so `Thinking`, `Waiting`, `Failed`, an unseen `Done`
/// and idle were told apart by hue alone — invisible to a colourblind reader
/// and to anyone running under `NO_COLOR`. The glyphs are ASCII plus `✓` and
/// the middle dot: single-width, no Nerd Font, no emoji.
pub(super) fn chat_status_glyph(status: ChatStatus) -> &'static str {
    match status {
        // Working.
        ChatStatus::Thinking => "*",
        // The agent asked something and is blocked on an answer.
        ChatStatus::Waiting => "?",
        ChatStatus::Failed => "!",
        // Finished and not looked at yet.
        ChatStatus::Done { seen: false } => "✓",
        ChatStatus::Done { seen: true } | ChatStatus::Idle => "·",
    }
}

/// Sidebar glyph for a terminal (E8). `$` is the ordinary shell prompt; a
/// running command, a clean exit and a failed exit each get their own shape, so
/// a crash is no longer "the same `$`, but red".
pub(super) fn terminal_glyph(terminal: &TerminalSession, pty_runtime: &PtyRuntime) -> &'static str {
    if terminal_has_active_command(terminal, pty_runtime) {
        return ">";
    }

    match pty_runtime.pty_exit_status(PtyKey::Terminal(terminal.id)) {
        Some(exit) if exit.code == 0 && exit.signal.is_none() => "✓",
        Some(_) => "!",
        None => "$",
    }
}

/// Color of the agent status dot in the sidebar. Blue (`pine`, running) and
/// gray (`muted`, inactive) are live states; green (`success`), yellow
/// (`gold`), and red (`love`) act as notifications that the agent wants the
/// user's attention. Green is suppressed once the finished agent has been seen
/// (`Done { seen: true }`); yellow and red persist until the status itself
/// changes — i.e. until a new prompt or an answered option moves the agent back
/// to running.
pub(super) fn chat_status_style(status: ChatStatus, palette: Palette) -> Style {
    let color = match status {
        ChatStatus::Thinking => palette.pine,
        ChatStatus::Waiting => palette.gold,
        ChatStatus::Failed => palette.love,
        ChatStatus::Done { seen: false } => palette.success,
        ChatStatus::Done { seen: true } | ChatStatus::Idle => palette.muted,
    };

    Style::default().fg(color)
}

pub(super) fn terminal_icon_style(
    terminal: &TerminalSession,
    pty_runtime: &PtyRuntime,
    focused: bool,
    palette: Palette,
) -> Style {
    let color = if terminal_has_active_command(terminal, pty_runtime) {
        palette.pine
    } else if let Some(exit) = pty_runtime.pty_exit_status(PtyKey::Terminal(terminal.id)) {
        if exit.code == 0 && exit.signal.is_none() {
            if focused {
                palette.muted
            } else {
                palette.success
            }
        } else {
            palette.love
        }
    } else {
        palette.muted
    };

    Style::default().fg(color)
}

pub(super) fn terminal_has_active_command(
    terminal: &TerminalSession,
    pty_runtime: &PtyRuntime,
) -> bool {
    matches!(terminal.launch, TerminalLaunch::Command(_))
        && pty_runtime.is_running(PtyKey::Terminal(terminal.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::AppLayout;
    use ratatui::{backend::TestBackend, Terminal};

    use crate::{
        config,
        model::{AgentKind, ChatId},
    };

    use super::super::{
        draw,
        test_support::{buffer_text, draw_text_with_config, test_palette},
    };

    #[test]
    fn chat_sidebar_label_tags_the_agent_kind() {
        let mut chat = ChatSession {
            id: ChatId::new(1).unwrap(),
            name: "agent".to_string(),
            status: ChatStatus::Idle,
            agent: AgentKind::Pi,
            legacy_messages: Vec::new(),
        };
        assert_eq!(chat_sidebar_label(&chat), "agent: pi");

        chat.agent = AgentKind::ClaudeCode;
        assert_eq!(chat_sidebar_label(&chat), "agent: cc");
    }
    #[test]
    fn terminal_display_label_uses_command_or_default_and_truncates() {
        let pty_runtime = PtyRuntime::new_offline();
        let command_terminal = TerminalSession {
            id: TerminalId::new(99).unwrap(),
            name: "cmd: ping".to_string(),
            restore_on_launch: true,
            legacy_status: None,
            launch: TerminalLaunch::Command("ping example.com".to_string()),
        };
        let shell_terminal = TerminalSession {
            id: TerminalId::new(100).unwrap(),
            name: "shell".to_string(),
            restore_on_launch: false,
            legacy_status: None,
            launch: TerminalLaunch::Shell,
        };

        assert_eq!(
            terminal_display_label(&command_terminal, &pty_runtime, 80),
            "ping example.com"
        );
        assert_eq!(
            terminal_display_label(&shell_terminal, &pty_runtime, 80),
            "terminal"
        );
        assert_eq!(
            terminal_display_label(&command_terminal, &pty_runtime, 8),
            "ping ex…"
        );

        let clear_terminal = TerminalSession {
            id: TerminalId::new(101).unwrap(),
            name: "clear".to_string(),
            restore_on_launch: false,
            legacy_status: None,
            launch: TerminalLaunch::Command("clear".to_string()),
        };
        assert_eq!(
            terminal_display_label(&clear_terminal, &pty_runtime, 80),
            "terminal"
        );
    }
    #[test]
    fn terminal_icon_color_tracks_active_commands_and_completion_focus() {
        let palette = test_palette();
        let pty_runtime = PtyRuntime::new_offline();
        let mut shell_terminal = TerminalSession {
            id: TerminalId::new(99).unwrap(),
            name: "shell".to_string(),
            restore_on_launch: false,
            legacy_status: None,
            launch: TerminalLaunch::Shell,
        };

        assert_eq!(
            terminal_icon_style(&shell_terminal, &pty_runtime, false, palette),
            Style::default().fg(palette.muted)
        );

        shell_terminal.restore_on_launch = true;
        assert_eq!(
            terminal_icon_style(&shell_terminal, &pty_runtime, false, palette),
            Style::default().fg(palette.muted)
        );

        let command_terminal = TerminalSession {
            id: TerminalId::new(100).unwrap(),
            name: "test".to_string(),
            restore_on_launch: true,
            legacy_status: None,
            launch: TerminalLaunch::Command("cargo test".to_string()),
        };
        let mut running_runtime = PtyRuntime::new_offline();
        running_runtime.mark_running_for_test(PtyKey::Terminal(command_terminal.id));
        assert_eq!(
            terminal_icon_style(&command_terminal, &running_runtime, false, palette),
            Style::default().fg(palette.pine)
        );

        let mut done_runtime = PtyRuntime::new_offline();
        done_runtime.record_exit_status_for_test(
            PtyKey::Terminal(command_terminal.id),
            crate::pty::PtyExit {
                code: 0,
                signal: None,
            },
        );
        assert_eq!(
            terminal_icon_style(&command_terminal, &done_runtime, false, palette),
            Style::default().fg(palette.success)
        );
        assert_eq!(
            terminal_icon_style(&command_terminal, &done_runtime, true, palette),
            Style::default().fg(palette.muted)
        );
    }
    #[test]
    fn agent_icon_color_tracks_chat_status() {
        let palette = test_palette();

        assert_eq!(
            chat_status_style(ChatStatus::Thinking, palette),
            Style::default().fg(palette.pine)
        );
        assert_eq!(
            chat_status_style(ChatStatus::Waiting, palette),
            Style::default().fg(palette.gold)
        );
        assert_eq!(
            chat_status_style(ChatStatus::Failed, palette),
            Style::default().fg(palette.love)
        );
        // Green only while the finished agent has not been seen; gray once seen.
        assert_eq!(
            chat_status_style(ChatStatus::Done { seen: false }, palette),
            Style::default().fg(palette.success)
        );
        assert_eq!(
            chat_status_style(ChatStatus::Done { seen: true }, palette),
            Style::default().fg(palette.muted)
        );
        assert_eq!(
            chat_status_style(ChatStatus::Idle, palette),
            Style::default().fg(palette.muted)
        );
    }
    #[test]
    fn default_sidebar_agent_icon_is_gray() {
        let app = App::seeded();
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &PtyRuntime::new_offline(),
                    &config::Config::default(),
                )
            })
            .expect("draw app");

        let palette = test_palette();
        let icon_cell = terminal
            .backend()
            .buffer()
            .cell((3, 1))
            .expect("chat icon is in bounds");
        assert_eq!(icon_cell.symbol(), "·");
        assert_eq!(icon_cell.fg, palette.muted);
    }
    #[test]
    fn selected_done_sidebar_agent_icon_is_gray() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.project.workspaces[0].chats[0].status = ChatStatus::Done { seen: false };
        app.select_item(NavItem::Chat { workspace, chat });

        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &PtyRuntime::new_offline(),
                    &config::Config::default(),
                )
            })
            .expect("draw app");

        let palette = test_palette();
        let icon_cell = terminal
            .backend()
            .buffer()
            .cell((3, 1))
            .expect("selected chat icon is in bounds");
        assert_eq!(icon_cell.symbol(), "·");
        assert_eq!(icon_cell.fg, palette.muted);
        assert_eq!(icon_cell.bg, palette.highlight_med);
    }
    #[test]
    fn waiting_sidebar_agent_icon_is_yellow() {
        let mut app = App::seeded();
        app.project.workspaces[0].chats[0].status = ChatStatus::Waiting;

        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &PtyRuntime::new_offline(),
                    &config::Config::default(),
                )
            })
            .expect("draw app");

        let palette = test_palette();
        let icon_cell = terminal
            .backend()
            .buffer()
            .cell((3, 1))
            .expect("chat icon is in bounds");
        assert_eq!(icon_cell.symbol(), "?");
        // Waiting (the agent is asking the user to pick an option) is yellow,
        // and stays yellow even while selected — only an answer clears it.
        assert_eq!(icon_cell.fg, palette.gold);
    }
    #[test]
    fn every_chat_state_has_its_own_glyph() {
        let glyphs = [
            chat_status_glyph(ChatStatus::Thinking),
            chat_status_glyph(ChatStatus::Waiting),
            chat_status_glyph(ChatStatus::Failed),
            chat_status_glyph(ChatStatus::Done { seen: false }),
            chat_status_glyph(ChatStatus::Idle),
        ];
        let unique = glyphs.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), glyphs.len(), "{glyphs:?}");
        // A seen `Done` is back to the inactive state in shape as well as hue.
        assert_eq!(
            chat_status_glyph(ChatStatus::Done { seen: true }),
            chat_status_glyph(ChatStatus::Idle)
        );
        // Single-width, so the sidebar columns do not shift per state.
        for glyph in glyphs {
            assert_eq!(text_width(glyph), 1, "{glyph}");
        }
    }
    #[test]
    fn a_crashed_terminal_does_not_look_like_a_clean_one() {
        let mut app = App::two_workspaces();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        let session = &app.project.workspaces[0].terminals[0];

        let mut pty_runtime = PtyRuntime::new_offline();
        assert_eq!(terminal_glyph(session, &pty_runtime), "$");

        pty_runtime.record_exit_status_for_test(
            PtyKey::Terminal(terminal),
            crate::pty::PtyExit {
                code: 0,
                signal: None,
            },
        );
        let clean = terminal_glyph(session, &pty_runtime);

        pty_runtime.record_exit_status_for_test(
            PtyKey::Terminal(terminal),
            crate::pty::PtyExit {
                code: 101,
                signal: None,
            },
        );
        let crashed = terminal_glyph(session, &pty_runtime);

        assert_ne!(clean, crashed);
        assert_eq!(text_width(clean), 1);
        assert_eq!(text_width(crashed), 1);
    }
    #[test]
    fn no_color_still_tells_the_sidebar_states_apart() {
        // Under `NO_COLOR` every foreground is `Color::Reset`, so before E8 the
        // whole sidebar was identical glyphs in identical colours. The shapes
        // are what makes that mode usable.
        let config = config::Config {
            color_output: config::ColorOutput::Disabled,
            ..config::Config::default()
        };

        let mut app = App::two_workspaces();
        let workspace = app.project.workspaces[0].id;
        app.project.workspaces[0].terminals.clear();
        let chat = app
            .project
            .add_chat(
                workspace,
                "a".to_string(),
                ChatStatus::Failed,
                AgentKind::Pi,
            )
            .expect("add chat");
        app.project
            .add_chat(
                workspace,
                "b".to_string(),
                ChatStatus::Waiting,
                AgentKind::Pi,
            )
            .expect("add chat");
        app.select_item(NavItem::Chat { workspace, chat });

        let text = draw_text_with_config(&app, &PtyRuntime::new_offline(), &config, 100, 8);

        assert!(text.contains("! a"), "{text}");
        assert!(text.contains("? b"), "{text}");
    }
    #[test]
    fn sidebar_renders_blank_row_between_workspace_groups() {
        let mut app = App::two_workspaces();
        app.project.workspaces[0].name = "first".to_string();
        app.project.workspaces[0].chats.clear();
        app.project.workspaces[0].terminals.clear();
        app.project.workspaces[1].name = "second".to_string();
        app.project.workspaces[1].chats.clear();
        app.project.workspaces[1].terminals.clear();

        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &PtyRuntime::new_offline(),
                    &config::Config::default(),
                )
            })
            .expect("draw app");

        assert!(buffer_text(terminal.backend(), 0, 0, 34).contains("▣ first"));
        assert!(buffer_text(terminal.backend(), 0, 1, 34).trim().is_empty());
        assert!(buffer_text(terminal.backend(), 0, 2, 34).contains("▣ second"));
    }
    #[test]
    fn sidebar_selection_skips_workspace_headers_and_spacers() {
        let mut app = App::seeded();
        let second_workspace = app.project.workspaces[1].id;
        let second_chat = app.project.workspaces[1].chats[0].id;
        app.select_item(NavItem::Chat {
            workspace: second_workspace,
            chat: second_chat,
        });

        let pty_runtime = PtyRuntime::new_offline();
        let rows = app.sidebar_rows();
        let items = sidebar_items(&app, &pty_runtime, test_palette(), 33, &rows);

        // The rows and the highlight come from one walk, so the highlight
        // cannot land on a header or a spacer however the order changes (F14).
        assert_eq!(items.len(), rows.len());
        assert_eq!(sidebar_highlight_row(&app, &rows), Some(6));
        assert!(matches!(rows[6], SidebarRow::Nav { .. }));
    }
    #[test]
    fn sidebar_workspace_branch_is_right_aligned() {
        let mut app = App::two_workspaces();
        let workspace = app.project.workspaces[0].id;
        app.project.workspaces.truncate(1);
        app.project.workspaces[0].name = "mult".to_string();
        app.project.workspaces[0].chats.clear();
        app.project.workspaces[0].terminals.clear();
        app.replace_workspace_git_branches([(workspace, Some("main".to_string()))]);

        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &PtyRuntime::new_offline(),
                    &config::Config::default(),
                )
            })
            .expect("draw app");

        let sidebar_row = buffer_text(terminal.backend(), 0, 0, 34);
        assert!(sidebar_row.contains("▣ mult"));
        assert!(sidebar_row.ends_with(" main "));
    }
}
