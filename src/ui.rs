use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::{
    app::{App, Mode, NavItem},
    model::{ChatStatus, TerminalId, TerminalLaunch, TerminalStatus, WorkspaceId},
    storage,
};

const FOOTER: &str = "q/esc quit • j/k move • o open/import • w workspace • c chat • t terminal • d command • s/x start/stop PTY • i input";
const TERMINAL_HEADER_LINES: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutAreas {
    sidebar: Rect,
    main: Rect,
    footer: Rect,
}

pub fn draw(frame: &mut Frame, app: &App) {
    let layout = layout_areas(app, frame.area());

    draw_sidebar(frame, app, layout.sidebar);
    draw_main(frame, app, layout.main);
    draw_footer(frame, app, layout.footer);
}

pub fn selected_terminal_output_area(app: &App, frame_area: Rect) -> Option<(TerminalId, Rect)> {
    let Some(NavItem::Terminal { terminal, .. }) = app.selected_item() else {
        return None;
    };

    let layout = layout_areas(app, frame_area);
    Some((terminal, terminal_output_area(layout.main)))
}

fn layout_areas(app: &App, frame_area: Rect) -> LayoutAreas {
    let footer_height = if app.is_prompt_active() { 4 } else { 1 };
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)]).areas(frame_area);
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(34), Constraint::Min(40)]).areas(body);

    LayoutAreas {
        sidebar,
        main,
        footer,
    }
}

fn terminal_output_area(main: Rect) -> Rect {
    let inner = bordered_inner(main);
    let header_height = TERMINAL_HEADER_LINES.min(inner.height);

    Rect {
        x: inner.x,
        y: inner.y.saturating_add(header_height),
        width: inner.width,
        height: inner.height.saturating_sub(header_height),
    }
}

fn bordered_inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let items = sidebar_items(app);
    let selected = (!items.is_empty()).then_some(app.selected.min(items.len() - 1));
    let mut state = ListState::default();
    state.select(selected);

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" workspaces "))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");

    frame.render_stateful_widget(list, area, &mut state);
}

fn sidebar_items(app: &App) -> Vec<ListItem<'static>> {
    app.project
        .workspaces
        .iter()
        .flat_map(|workspace| {
            let workspace_line = ListItem::new(Line::from(vec![
                Span::styled("▣ ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    workspace.name.clone(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            let chat_lines = workspace.chats.iter().map(move |chat| {
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("● ", chat_status_style(chat.status)),
                    Span::raw(chat.name.clone()),
                    Span::styled(
                        format!(" [{}]", chat.status.label()),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            });

            let terminal_lines = workspace.terminals.iter().map(move |terminal| {
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("$ ", terminal_status_style(terminal.status)),
                    Span::raw(terminal.name.clone()),
                    Span::styled(
                        format!(" [{}]", terminal.status.label()),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            });

            std::iter::once(workspace_line)
                .chain(chat_lines)
                .chain(terminal_lines)
                .collect::<Vec<_>>()
                .into_iter()
        })
        .collect()
}

fn draw_main(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" mult — AI agent multiplexer ");

    let terminal_output_rows = usize::from(terminal_output_area(area).height.max(1));
    let lines = match app.selected_item() {
        Some(NavItem::Workspace(workspace)) => workspace_details(app, workspace),
        Some(NavItem::Chat { workspace, chat }) => chat_details(app, workspace, chat),
        Some(NavItem::Terminal {
            workspace,
            terminal,
        }) => terminal_details(app, workspace, terminal, terminal_output_rows),
        None => vec![Line::from("No workspaces yet.")],
    };

    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn workspace_details(app: &App, workspace_id: WorkspaceId) -> Vec<Line<'static>> {
    let Some(workspace) = app.project.workspace(workspace_id) else {
        return vec![Line::from("Missing workspace.")];
    };

    let cwd = workspace
        .cwd
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unset>".to_string());

    vec![
        Line::from(vec![
            Span::styled("Workspace: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                workspace.name.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Id: ", Style::default().fg(Color::DarkGray)),
            Span::raw(workspace.id.0.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Cwd: ", Style::default().fg(Color::DarkGray)),
            Span::raw(cwd),
        ]),
        Line::from(format!("Env vars: {}", workspace.environment.len())),
        Line::from(format!("Chats: {}", workspace.chats.len())),
        Line::from(format!("Terminals: {}", workspace.terminals.len())),
        Line::from(""),
        Line::from(vec![
            Span::styled("State file: ", Style::default().fg(Color::DarkGray)),
            Span::raw(storage::state_path().display().to_string()),
        ]),
        Line::from(""),
        Line::from("M1 is nearly complete: stable IDs, cwd/env metadata, JSON persistence, and open/import."),
        Line::from("Press `o` to import another workspace by directory path."),
    ]
}

fn chat_details(
    app: &App,
    workspace_id: WorkspaceId,
    chat_id: crate::model::ChatId,
) -> Vec<Line<'static>> {
    let Some(workspace) = app.project.workspace(workspace_id) else {
        return vec![Line::from("Missing workspace.")];
    };
    let Some(chat) = app.project.chat(workspace_id, chat_id) else {
        return vec![Line::from("Missing chat.")];
    };

    vec![
        Line::from(vec![
            Span::styled("Chat: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                chat.name.clone(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Id: ", Style::default().fg(Color::DarkGray)),
            Span::raw(chat.id.0.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Workspace: ", Style::default().fg(Color::DarkGray)),
            Span::raw(workspace.name.clone()),
        ]),
        Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
            Span::styled(chat.status.label(), chat_status_style(chat.status)),
        ]),
        Line::from(""),
        Line::from("Placeholder transcript"),
        Line::from("──────────────────────"),
        Line::from("user  > sketch the multiplexer architecture"),
        Line::from("agent > building the shell: state, sidebar, detail pane, and keybindings"),
        Line::from(""),
        Line::from("The real implementation will stream agent output into this pane."),
    ]
}

fn terminal_details(
    app: &App,
    workspace_id: WorkspaceId,
    terminal_id: crate::model::TerminalId,
    output_rows: usize,
) -> Vec<Line<'static>> {
    let Some(workspace) = app.project.workspace(workspace_id) else {
        return vec![Line::from("Missing workspace.")];
    };
    let Some(terminal) = app.project.terminal(workspace_id, terminal_id) else {
        return vec![Line::from("Missing terminal.")];
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Terminal: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                terminal.name.clone(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Id: ", Style::default().fg(Color::DarkGray)),
            Span::raw(terminal.id.0.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Workspace: ", Style::default().fg(Color::DarkGray)),
            Span::raw(workspace.name.clone()),
        ]),
        Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                terminal.status.label(),
                terminal_status_style(terminal.status),
            ),
        ]),
        Line::from(vec![
            Span::styled("Launch: ", Style::default().fg(Color::DarkGray)),
            Span::raw(launch_label(&terminal.launch)),
        ]),
        Line::from(""),
        Line::from("Controls: s start • x stop • i focus input • Esc unfocus"),
        Line::from(""),
        Line::from("PTY output"),
        Line::from("──────────────────────"),
    ];

    let output = app.terminal_lines(terminal_id);
    if output.is_empty() {
        lines.push(Line::from(
            "PTY not started. Select this terminal and press `s`.",
        ));
        lines.push(Line::from(
            "After starting, press `i` to focus terminal input.",
        ));
    } else {
        lines.extend(
            output
                .into_iter()
                .rev()
                .take(output_rows)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(Line::from),
        );
    }

    lines
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    match &app.mode {
        Mode::Normal => {
            let footer = Paragraph::new(FOOTER).style(Style::default().fg(Color::DarkGray));
            frame.render_widget(footer, area);
        }
        Mode::OpenWorkspace(prompt) => {
            draw_text_prompt(
                frame,
                area,
                " open workspace ",
                "Path: ",
                &prompt.input,
                prompt.error.as_deref(),
                "enter imports • esc/ctrl-c cancels",
            );
        }
        Mode::NewTerminalCommand(prompt) => {
            draw_text_prompt(
                frame,
                area,
                " new command terminal ",
                "Command: ",
                &prompt.input,
                prompt.error.as_deref(),
                "enter adds command terminal • esc/ctrl-c cancels",
            );
        }
        Mode::TerminalInput { .. } => {
            let footer = Paragraph::new("terminal input focused • typing goes to PTY • Esc returns to mult • Ctrl-C sends interrupt")
                .style(Style::default().fg(Color::Yellow));
            frame.render_widget(footer, area);
        }
    }
}

fn draw_text_prompt(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    label: &'static str,
    input: &str,
    error: Option<&str>,
    help: &'static str,
) {
    let message = error.unwrap_or(help);
    let message_style = if error.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let prompt = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(label, Style::default().fg(Color::DarkGray)),
            Span::raw(input.to_string()),
            Span::styled("▌", Style::default().fg(Color::Yellow)),
        ]),
        Line::from(Span::styled(message.to_string(), message_style)),
    ])
    .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(prompt, area);
}

fn launch_label(launch: &TerminalLaunch) -> String {
    match launch {
        TerminalLaunch::Shell => "shell".to_string(),
        TerminalLaunch::Command(command) => format!("command: {command}"),
    }
}

fn chat_status_style(status: ChatStatus) -> Style {
    let color = match status {
        ChatStatus::Idle => Color::Blue,
        ChatStatus::Thinking => Color::Yellow,
        ChatStatus::Waiting => Color::Magenta,
        ChatStatus::Done => Color::Green,
    };

    Style::default().fg(color)
}

fn terminal_status_style(status: TerminalStatus) -> Style {
    let color = match status {
        TerminalStatus::Stopped => Color::Red,
        TerminalStatus::Running => Color::Green,
    };

    Style::default().fg(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_terminal_output_area_tracks_visible_main_pane_size() {
        let mut app = App::default();
        app.selected = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Terminal { .. }))
            .expect("seed state has a terminal");

        let (_, area) = selected_terminal_output_area(&app, Rect::new(0, 0, 120, 40))
            .expect("terminal selection has output area");

        assert_eq!(area.width, 84);
        assert_eq!(area.height, 27);
    }

    #[test]
    fn selected_terminal_output_area_is_absent_for_non_terminal_selection() {
        let app = App::default();

        assert_eq!(
            selected_terminal_output_area(&app, Rect::new(0, 0, 120, 40)),
            None
        );
    }
}
