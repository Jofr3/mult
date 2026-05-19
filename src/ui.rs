use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::{
    app::{App, Mode, NavItem},
    model::{ChatStatus, TerminalStatus, WorkspaceId},
    storage,
};

const FOOTER: &str = "q/esc quit • j/k move • o open/import • w workspace • c chat • t terminal • s/x start/stop PTY • r status";

pub fn draw(frame: &mut Frame, app: &App) {
    let footer_height = if app.is_open_workspace_prompt_active() {
        4
    } else {
        1
    };
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)])
        .areas(frame.area());
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(34), Constraint::Min(40)]).areas(body);

    draw_sidebar(frame, app, sidebar);
    draw_main(frame, app, main);
    draw_footer(frame, app, footer);
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

    let lines = match app.selected_item() {
        Some(NavItem::Workspace(workspace)) => workspace_details(app, workspace),
        Some(NavItem::Chat { workspace, chat }) => chat_details(app, workspace, chat),
        Some(NavItem::Terminal {
            workspace,
            terminal,
        }) => terminal_details(app, workspace, terminal),
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
        Line::from(""),
        Line::from("Controls: s start shell • x stop shell"),
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
            "This slice captures shell output; keyboard input routing comes next.",
        ));
    } else {
        lines.extend(
            output
                .into_iter()
                .rev()
                .take(20)
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
            let error = prompt
                .error
                .as_deref()
                .unwrap_or("enter imports • esc/ctrl-c cancels");
            let error_style = if prompt.error.is_some() {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let prompt = Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Path: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(prompt.input.clone()),
                    Span::styled("▌", Style::default().fg(Color::Yellow)),
                ]),
                Line::from(Span::styled(error.to_string(), error_style)),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" open workspace "),
            );
            frame.render_widget(prompt, area);
        }
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
