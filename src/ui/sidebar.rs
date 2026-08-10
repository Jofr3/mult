//! The sidebar: workspaces, their chats and terminals, and the status glyphs
//! that carry chat and terminal state by shape as well as by colour (E8).

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

use super::main_pane::{focus_is_active, pane_style};
use super::text::{text_width, truncate_text};
use super::theme::Palette;

const SIDEBAR_SELECTION_SYMBOL: &str = " ";

const WORKSPACE_ICON: &str = "▣ ";

const GIT_BRANCH_ICON: &str = "";

pub(super) fn draw_sidebar(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    area: Rect,
    palette: Palette,
) {
    let rows = app.sidebar_rows();
    let items = sidebar_items(app, pty_runtime, palette, sidebar_item_width(area), &rows);
    let selected = sidebar_highlight_row(app, &rows);
    let mut state = ListState::default();
    state.select(selected);

    let focused = focus_is_active(app, FocusMode::Sidebar);
    let style = pane_style(focused, palette);
    frame.render_widget(Block::default().style(style), area);

    let list = List::new(items)
        .style(style)
        .highlight_style(palette.selection_highlight())
        .highlight_symbol(SIDEBAR_SELECTION_SYMBOL)
        .highlight_spacing(HighlightSpacing::Always);

    frame.render_stateful_widget(list, area, &mut state);
}

/// Which of `rows` carries the selection highlight.
///
/// Found by position in the rows themselves, so headers and spacers cannot
/// shift it (F14): it is the `selected_index()`-th selectable row. With no
/// selection the first selectable row is highlighted, as it always was.
fn sidebar_highlight_row(app: &App, rows: &[SidebarRow]) -> Option<usize> {
    let target_nav_index = app.selected_index().unwrap_or(0);
    rows.iter()
        .enumerate()
        .filter(|(_, row)| matches!(row, SidebarRow::Nav(_)))
        .nth(target_nav_index)
        .map(|(index, _)| index)
}

/// Render the rows `App::sidebar_rows` produced, one `ListItem` each. The
/// order is the model's, not this function's: a row it cannot resolve still
/// occupies its index so the highlight stays aligned.
fn sidebar_items(
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
            SidebarRow::Nav(NavItem::Chat { workspace, chat }) => {
                match app.project.chat(*workspace, *chat) {
                    Some(chat) => {
                        let (glyph, style) = chat_status_marker(chat.status, palette);
                        ListItem::new(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(glyph, style),
                            // The same four columns the terminal rows subtract:
                            // two of indent plus the two-column status glyph.
                            Span::raw(chat_sidebar_label(
                                chat,
                                pty_runtime,
                                item_width.saturating_sub(4),
                            )),
                        ]))
                    }
                    None => ListItem::new(Line::from("")),
                }
            }
            SidebarRow::Nav(NavItem::Terminal {
                workspace,
                terminal,
            }) => match app.project.terminal(*workspace, *terminal) {
                Some(terminal) => {
                    let focused = terminal_sidebar_item_is_focused(app, *workspace, terminal.id);
                    let (glyph, style) =
                        terminal_icon_marker(terminal, pty_runtime, focused, palette);
                    ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(glyph, style),
                        Span::raw(terminal_display_label(
                            terminal,
                            pty_runtime,
                            item_width.saturating_sub(4),
                        )),
                    ]))
                }
                None => ListItem::new(Line::from("")),
            },
        })
        .collect()
}

fn terminal_sidebar_item_is_focused(
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

fn sidebar_item_width(area: Rect) -> usize {
    usize::from(
        area.width
            .saturating_sub(text_width(SIDEBAR_SELECTION_SYMBOL) as u16),
    )
}

fn workspace_sidebar_line(
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

/// Sidebar label for a chat: the agent's own window title (OSC 0/2) once it
/// sets one, and the chat's name until then.
///
/// Nothing is overridden by that title. Every chat is created with the same
/// `DEFAULT_AGENT_CHAT_TITLE` and there is no way to rename one, so three
/// Claude Code chats in a workspace would otherwise be three identical rows.
/// What the agent says it is working on is what tells them apart.
fn chat_sidebar_label(chat: &ChatSession, pty_runtime: &PtyRuntime, max_width: usize) -> String {
    let name = pty_runtime
        .terminal_title(PtyKey::ChatAgent(chat.id))
        .unwrap_or_else(|| chat.name.clone());
    truncate_text(&name, max_width)
}

fn terminal_display_label(
    terminal: &TerminalSession,
    pty_runtime: &PtyRuntime,
    max_width: usize,
) -> String {
    truncate_text(&terminal_command_label(terminal, pty_runtime), max_width)
}

/// What a terminal row is called.
///
/// A **command** terminal keeps the command it was created with. That string is
/// the user's own answer to "which pane is this", typed into the new-terminal
/// prompt, and a program that renames its window every rebuild would move the
/// landmark out from under them. The command is theirs; it is not `mult`'s to
/// replace.
///
/// A **shell** terminal has no such answer, so its label is derived, and the
/// three sources it is derived from answer different questions. While a command
/// is running, what the pane *is* is that command, so [`base_command`] of it
/// wins: a row reading `~/projects/mult` says nothing a user watching one pane
/// build and another test can use. At the prompt there is no command to name,
/// and the program's own window title is the better answer — a shell writes its
/// `cwd` there on every prompt and an editor writes the file it is on, and
/// neither is a guess, unlike [`PtyRuntime::terminal_last_command`], which
/// guesses by watching keystrokes go past. The scrape stays last, for the shell
/// that never sets a title at all.
fn terminal_command_label(terminal: &TerminalSession, pty_runtime: &PtyRuntime) -> String {
    let key = PtyKey::Terminal(terminal.id);
    match &terminal.launch {
        TerminalLaunch::Command(command) => command_label_or_default(command),
        TerminalLaunch::Shell => running_command_label(key, pty_runtime)
            .or_else(|| pty_runtime.terminal_title(key))
            .or_else(|| {
                pty_runtime
                    .terminal_last_command(key)
                    .and_then(reduced_command_label)
            })
            .unwrap_or_else(|| "terminal".to_string()),
    }
}

/// The command a shell pane is running right now, reduced to its base, or
/// `None` when the shell is at its prompt.
///
/// Gated on the daemon's foreground-process report rather than on the tracker
/// alone: the tracker keeps the last command *seen*, which outlives it by the
/// whole idle stretch after it exits, so without the gate a pane would keep
/// claiming to run `cargo test` until something else was typed.
fn running_command_label(key: PtyKey, pty_runtime: &PtyRuntime) -> Option<String> {
    if !pty_runtime.terminal_runs_child_command(key) {
        return None;
    }
    pty_runtime
        .terminal_last_command(key)
        .and_then(reduced_command_label)
}

/// [`base_command`] as a label, or `None` when what is left says nothing worth
/// displacing the window title for.
fn reduced_command_label(command: &str) -> Option<String> {
    let label = base_command(command);
    (!is_uninformative_command(&label)).then_some(label)
}

/// A command line reduced to the part that names what is running: everything
/// before its first flag, with any leading `KEY=value` dropped.
///
/// The sidebar has room for a word or two, and the flags are the part a user
/// scanning rows already knows — `cargo test --workspace --all-targets` and
/// `cargo test -p mult` are both "the tests". Cutting at the first flag rather
/// than deleting flags in place is what keeps a subcommand: deletion strands
/// the flag's *value*, since `git commit -m 'msg'` holds the message in a token
/// that does not start with `-`, and would render as `git commit 'msg'`. The
/// cut takes the tail with it, so a pipeline (`ls -la | wc -l`) goes too.
///
/// A command with no flags is kept whole, because then there is nothing to cut
/// and every token is load-bearing: `sudo apt update` stays itself, and so does
/// the file an editor was opened on. Width is [`truncate_text`]'s job, not this
/// one's.
fn base_command(command: &str) -> String {
    command
        .split_whitespace()
        .skip_while(|token| is_env_assignment(token))
        .take_while(|token| !token.starts_with('-'))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `token` is a `KEY=value` prefix rather than the command itself.
///
/// `FOO=1 cargo test` is `cargo test` with an environment set for it, and the
/// assignment is exactly the noise this is here to drop. A quote anywhere in
/// the token disqualifies it: `FOO='a b'` was split across two tokens by
/// whitespace before it got here, and half an assignment is not one.
fn is_env_assignment(token: &str) -> bool {
    if token.contains('\'') || token.contains('"') {
        return false;
    }
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(|ch: char| ch.is_ascii_digit())
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn command_label_or_default(command: &str) -> String {
    let command = command.trim();
    if is_uninformative_command(command) {
        "terminal".to_string()
    } else {
        command.to_string()
    }
}

/// A command that names nothing: the empty string, and `clear`, which every
/// pane runs and no pane is.
fn is_uninformative_command(command: &str) -> bool {
    command.is_empty() || command == "clear"
}

/// The agent status marker in the sidebar: glyph first, colour second (E8).
///
/// Every chat used to render the identical `"● "`, with the whole signal in the
/// hue, so a red-green colourblind user could not tell a finished agent from a
/// failed one — and with `NO_COLOR` (E10) nobody could. The shape now carries
/// the state and the colour reinforces it. All six glyphs are single-width and
/// long-standing Unicode; none needs a Nerd Font or an emoji font.
///
/// Blue (`pine`, running) and gray (`muted`, inactive) are live states; green
/// (`success`), yellow (`gold`) and red (`love`) act as notifications that the
/// agent wants the user's attention. Green is suppressed once the finished
/// agent has been seen (`done_seen`); yellow and red persist until the status
/// itself changes — i.e. until a new prompt or an answered option moves the
/// agent back to running.
fn chat_status_marker(status: ChatStatus, palette: Palette) -> (&'static str, Style) {
    let (glyph, color, emphatic) = match status {
        // half-filled: work in progress
        ChatStatus::Thinking => ("◐ ", palette.pine, false),
        // a question: the agent is asking the user to choose
        ChatStatus::Waiting => ("? ", palette.gold, true),
        ChatStatus::Failed => ("✗ ", palette.love, true),
        // a tick only while the finish has not been acknowledged
        ChatStatus::Done => ("✓ ", palette.success, true),
        // settled: filled for a seen finish, hollow for never-started
        ChatStatus::DoneSeen => ("● ", palette.muted, false),
        ChatStatus::Idle => ("○ ", palette.muted, false),
    };

    (glyph, palette.accent(color, emphatic))
}

/// The terminal marker in the sidebar, on the same principle as
/// [`chat_status_marker`]: `>` is running, `✓`/`✗` are how it ended, `$` is a
/// terminal that has not run anything worth reporting.
fn terminal_icon_marker(
    terminal: &TerminalSession,
    pty_runtime: &PtyRuntime,
    focused: bool,
    palette: Palette,
) -> (&'static str, Style) {
    let (glyph, color, emphatic) = if terminal_has_active_command(terminal, pty_runtime) {
        ("> ", palette.pine, false)
    } else if let Some(exit) = pty_runtime.terminal_exit_status(PtyKey::Terminal(terminal.id)) {
        if exit.code == 0 && exit.signal.is_none() {
            // A clean exit the user is already looking at is not news.
            if focused {
                ("✓ ", palette.muted, false)
            } else {
                ("✓ ", palette.success, true)
            }
        } else {
            ("✗ ", palette.love, true)
        }
    } else {
        ("$ ", palette.muted, false)
    };

    (glyph, palette.accent(color, emphatic))
}

fn terminal_has_active_command(terminal: &TerminalSession, pty_runtime: &PtyRuntime) -> bool {
    matches!(terminal.launch, TerminalLaunch::Command(_))
        && pty_runtime.is_running(PtyKey::Terminal(terminal.id))
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    use mult_protocol::ForegroundProcessInfo;

    use crate::config;
    use crate::model::AgentKind;
    use crate::model::ChatId;
    use crate::ui::test_support::*;

    fn test_chat(agent: AgentKind) -> ChatSession {
        ChatSession {
            id: ChatId(1),
            name: "agent".to_string(),
            status: ChatStatus::Idle,
            agent,
            messages: Vec::new(),
        }
    }

    #[test]
    fn a_chat_row_follows_the_agents_own_window_title() {
        // Every chat is created with the same name and none can be renamed, so
        // three Claude Code chats would otherwise be three identical rows.
        let chat = test_chat(AgentKind::ClaudeCode);
        let key = PtyKey::ChatAgent(chat.id);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.ensure_parser(key, crate::pty::PtyDimensions { rows: 24, cols: 80 });
        pty_runtime.process_terminal_output(key, b"\x1b]0;fix the parser\x07");

        assert_eq!(
            chat_sidebar_label(&chat, &pty_runtime, 40),
            "fix the parser"
        );
        assert_eq!(chat_sidebar_label(&chat, &pty_runtime, 12), "fix the par…");

        // A title the program blanks out returns the row to the chat's name.
        pty_runtime.process_terminal_output(key, b"\x1b]0;\x07");
        assert_eq!(chat_sidebar_label(&chat, &pty_runtime, 40), "agent");
    }

    #[test]
    fn terminal_display_label_uses_command_or_default_and_truncates() {
        let pty_runtime = PtyRuntime::new_offline();
        let command_terminal = TerminalSession {
            id: TerminalId(99),
            name: "cmd: ping".to_string(),
            restore_on_launch: true,
            launch: TerminalLaunch::Command("ping example.com".to_string()),
        };
        let shell_terminal = TerminalSession {
            id: TerminalId(100),
            name: "shell".to_string(),
            restore_on_launch: false,
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
            id: TerminalId(101),
            name: "clear".to_string(),
            restore_on_launch: false,
            launch: TerminalLaunch::Command("clear".to_string()),
        };
        assert_eq!(
            terminal_display_label(&clear_terminal, &pty_runtime, 80),
            "terminal"
        );
    }

    #[test]
    fn a_shell_row_prefers_the_programs_window_title_over_the_scraped_command() {
        let shell_terminal = TerminalSession {
            id: TerminalId(100),
            name: "shell".to_string(),
            restore_on_launch: false,
            launch: TerminalLaunch::Shell,
        };
        let key = PtyKey::Terminal(shell_terminal.id);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.ensure_parser(key, crate::pty::PtyDimensions { rows: 24, cols: 80 });

        // Nothing to go on yet.
        assert_eq!(
            terminal_display_label(&shell_terminal, &pty_runtime, 80),
            "terminal"
        );

        // The scrape is the fallback it always was...
        pty_runtime.record_command_for_test(key, b"htop\r");
        assert_eq!(
            terminal_display_label(&shell_terminal, &pty_runtime, 80),
            "htop"
        );

        // ...but at the prompt the program's own statement wins, because it is
        // not a guess. OSC 2 and OSC 0 both set the window title; ST terminates
        // as well as BEL.
        pty_runtime.process_terminal_output(key, b"\x1b]2;~/projects/mult\x1b\\");
        assert_eq!(
            terminal_display_label(&shell_terminal, &pty_runtime, 80),
            "~/projects/mult"
        );

        // A command terminal keeps the command the user created it with: that
        // string is their landmark for the pane, not `mult`'s to overwrite.
        let command_terminal = TerminalSession {
            id: TerminalId(99),
            name: "cmd: ping".to_string(),
            restore_on_launch: true,
            launch: TerminalLaunch::Command("ping example.com".to_string()),
        };
        pty_runtime.ensure_parser(
            PtyKey::Terminal(command_terminal.id),
            crate::pty::PtyDimensions { rows: 24, cols: 80 },
        );
        pty_runtime.process_terminal_output(
            PtyKey::Terminal(command_terminal.id),
            b"\x1b]0;something else\x07",
        );
        assert_eq!(
            terminal_display_label(&command_terminal, &pty_runtime, 80),
            "ping example.com"
        );
    }

    #[test]
    fn a_shell_row_names_the_command_it_is_running_over_the_window_title() {
        let shell_terminal = TerminalSession {
            id: TerminalId(100),
            name: "shell".to_string(),
            restore_on_launch: false,
            launch: TerminalLaunch::Shell,
        };
        let key = PtyKey::Terminal(shell_terminal.id);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime.ensure_parser(key, crate::pty::PtyDimensions { rows: 24, cols: 80 });
        pty_runtime.process_terminal_output(key, b"\x1b]2;~/projects/mult\x1b\\");

        // At the prompt the title is the whole answer.
        assert_eq!(
            terminal_display_label(&shell_terminal, &pty_runtime, 80),
            "~/projects/mult"
        );

        // Running something, the row is that something — and the flags are the
        // part the user scanning rows already knows.
        pty_runtime.record_foreground_process_for_test(
            key,
            ForegroundProcessInfo {
                root_pid: Some(10),
                foreground_pid: Some(20),
                command: Some("ls -la".to_string()),
            },
        );
        assert_eq!(
            terminal_display_label(&shell_terminal, &pty_runtime, 80),
            "ls"
        );

        // Back at the prompt the command is over, and the title — which the
        // shell has been keeping current all along — takes the row back. The
        // tracker still holds `ls -la`, so nothing but the gate stops the row
        // claiming to run it forever.
        pty_runtime.record_foreground_process_for_test(
            key,
            ForegroundProcessInfo {
                root_pid: Some(10),
                foreground_pid: Some(10),
                command: Some("bash".to_string()),
            },
        );
        assert_eq!(pty_runtime.terminal_last_command(key), Some("ls -la"));
        assert_eq!(
            terminal_display_label(&shell_terminal, &pty_runtime, 80),
            "~/projects/mult"
        );
    }

    #[test]
    fn a_base_command_is_everything_before_the_first_flag() {
        // Cut at the first flag, not "delete the flags": deletion would strand
        // `-m`'s value and render `git commit 'msg'`.
        assert_eq!(base_command("ls -la"), "ls");
        assert_eq!(base_command("git commit -m 'msg'"), "git commit");
        assert_eq!(base_command("cargo test --workspace"), "cargo test");
        assert_eq!(base_command("docker run -it ubuntu bash"), "docker run");

        // The cut takes the tail with it, pipeline included.
        assert_eq!(base_command("ls -la | wc -l"), "ls");

        // Nothing to cut: every token is load-bearing, and no wrapper needs a
        // special case to survive.
        assert_eq!(base_command("sudo apt update"), "sudo apt update");
        assert_eq!(base_command("nvim src/pty.rs"), "nvim src/pty.rs");

        // A leading environment is set *for* the command; it is not the command.
        assert_eq!(base_command("RUST_LOG=debug cargo run"), "cargo run");
        assert_eq!(base_command("A=1 B=2 make"), "make");
        // ...but a token that only looks like one is left alone: `=` inside an
        // argument is not an assignment, and a quoted value the whitespace
        // split already cut in half is not one either. Keeping the line whole
        // is the readable failure there; dropping `FOO='a` would leave the row
        // reading `b' cargo run`.
        assert_eq!(base_command("./configure=x"), "./configure=x");
        assert_eq!(base_command("FOO='a b' cargo run"), "FOO='a b' cargo run");

        // Nothing left to name falls through to the caller's fallback.
        assert_eq!(base_command("-la"), "");
        assert_eq!(base_command("   "), "");
        assert_eq!(reduced_command_label("clear"), None);
        assert_eq!(reduced_command_label("-x"), None);
        assert_eq!(reduced_command_label("htop -d 5"), Some("htop".to_string()));
    }

    #[test]
    fn terminal_icon_shape_and_color_track_active_commands_and_completion_focus() {
        let palette = test_palette();
        let pty_runtime = PtyRuntime::new_offline();
        let mut shell_terminal = TerminalSession {
            id: TerminalId(99),
            name: "shell".to_string(),
            restore_on_launch: false,
            launch: TerminalLaunch::Shell,
        };

        assert_eq!(
            terminal_icon_marker(&shell_terminal, &pty_runtime, false, palette),
            ("$ ", Style::default().fg(palette.muted))
        );

        shell_terminal.restore_on_launch = true;
        assert_eq!(
            terminal_icon_marker(&shell_terminal, &pty_runtime, false, palette),
            ("$ ", Style::default().fg(palette.muted))
        );

        let command_terminal = TerminalSession {
            id: TerminalId(100),
            name: "test".to_string(),
            restore_on_launch: true,
            launch: TerminalLaunch::Command("cargo test".to_string()),
        };
        let mut running_runtime = PtyRuntime::new_offline();
        running_runtime.mark_running_for_test(PtyKey::Terminal(command_terminal.id));
        assert_eq!(
            terminal_icon_marker(&command_terminal, &running_runtime, false, palette),
            ("> ", Style::default().fg(palette.pine))
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
            terminal_icon_marker(&command_terminal, &done_runtime, false, palette),
            ("\u{2713} ", Style::default().fg(palette.success))
        );
        assert_eq!(
            terminal_icon_marker(&command_terminal, &done_runtime, true, palette),
            ("\u{2713} ", Style::default().fg(palette.muted))
        );

        let mut failed_runtime = PtyRuntime::new_offline();
        failed_runtime.record_exit_status_for_test(
            PtyKey::Terminal(command_terminal.id),
            crate::pty::PtyExit {
                code: 1,
                signal: None,
            },
        );
        // E8: a crash and a clean exit differ in shape, not only in hue.
        assert_eq!(
            terminal_icon_marker(&command_terminal, &failed_runtime, false, palette),
            ("\u{2717} ", Style::default().fg(palette.love))
        );
    }

    #[test]
    fn agent_icon_shape_and_color_track_chat_status() {
        let palette = test_palette();

        assert_eq!(
            chat_status_marker(ChatStatus::Thinking, palette),
            ("\u{25d0} ", Style::default().fg(palette.pine))
        );
        assert_eq!(
            chat_status_marker(ChatStatus::Waiting, palette),
            ("? ", Style::default().fg(palette.gold))
        );
        assert_eq!(
            chat_status_marker(ChatStatus::Failed, palette),
            ("\u{2717} ", Style::default().fg(palette.love))
        );
        // Green only while the finished agent has not been seen; gray once seen.
        assert_eq!(
            chat_status_marker(ChatStatus::Done, palette),
            ("\u{2713} ", Style::default().fg(palette.success))
        );
        assert_eq!(
            chat_status_marker(ChatStatus::DoneSeen, palette),
            ("\u{25cf} ", Style::default().fg(palette.muted))
        );
        assert_eq!(
            chat_status_marker(ChatStatus::Idle, palette),
            ("\u{25cb} ", Style::default().fg(palette.muted))
        );
    }

    #[test]
    fn every_status_is_a_distinct_single_width_glyph() {
        // E8: colour must never be the only carrier of state. Two states that
        // share a glyph would be indistinguishable to a colourblind user and
        // under `NO_COLOR`, and a double-width glyph would shift the label of
        // that one row.
        let palette = test_palette();
        let markers = [
            chat_status_marker(ChatStatus::Idle, palette).0,
            chat_status_marker(ChatStatus::Thinking, palette).0,
            chat_status_marker(ChatStatus::Waiting, palette).0,
            chat_status_marker(ChatStatus::Done, palette).0,
            chat_status_marker(ChatStatus::DoneSeen, palette).0,
            chat_status_marker(ChatStatus::Failed, palette).0,
        ];
        let unique = markers.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            unique.len(),
            markers.len(),
            "{markers:?} are not all distinct"
        );
        for marker in markers {
            assert_eq!(
                text_width(marker),
                2,
                "{marker:?} is not one glyph plus a space"
            );
        }
    }

    #[test]
    fn default_sidebar_agent_icon_is_a_gray_hollow_circle() {
        let app = App::seeded();
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                draw_app(
                    frame,
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
        assert_eq!(icon_cell.symbol(), "○");
        assert_eq!(icon_cell.fg, palette.muted);
    }

    #[test]
    fn selected_done_sidebar_agent_icon_is_a_gray_filled_circle() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.project.workspaces[0].chats[0].status = ChatStatus::Done;
        app.select_item(NavItem::Chat { workspace, chat });

        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                draw_app(
                    frame,
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
        // Seen-and-finished: filled, settled, gray — distinct in shape from
        // the hollow "never started" circle and from the tick of an unseen
        // finish.
        assert_eq!(icon_cell.symbol(), "●");
        assert_eq!(icon_cell.fg, palette.muted);
        assert_eq!(icon_cell.bg, palette.highlight_med);
    }

    #[test]
    fn waiting_sidebar_agent_icon_is_a_yellow_question_mark() {
        let mut app = App::seeded();
        app.project.workspaces[0].chats[0].status = ChatStatus::Waiting;

        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                draw_app(
                    frame,
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
    fn sidebar_renders_blank_row_between_workspace_groups() {
        let mut app = App::default();
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
                draw_app(
                    frame,
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

        // One row per model row, and the highlight lands on the chat itself,
        // past the first group, the spacer and the second header.
        assert_eq!(items.len(), rows.len());
        assert_eq!(sidebar_highlight_row(&app, &rows), Some(6));
        assert!(matches!(
            rows[6],
            SidebarRow::Nav(NavItem::Chat { chat, .. }) if chat == second_chat
        ));
    }

    #[test]
    fn sidebar_workspace_branch_is_right_aligned() {
        let mut app = App::default();
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
                draw_app(
                    frame,
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
