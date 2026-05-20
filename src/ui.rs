use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::{
    app::{
        chat_agent_terminal_id, App, FocusMode, Mode, NavItem, Prompt, TerminalCellStyle,
        TerminalColor, TerminalRenderLine,
    },
    config::{self, ColorSchemeConfig},
    model::{
        ChatId, ChatStatus, TerminalId, TerminalLaunch, TerminalSession, TerminalStatus,
        WorkspaceId,
    },
};

const FOOTER: &str = "j/k nav/scroll • wheel/pgup/pgdn output • home/end top/bottom • enter pane • esc sidebar • n a agent • n t terminal • n c command • n w workspace • d d delete • i input • q quit";
const CHAT_AGENT_HEADER_LINES: u16 = 0;
const TERMINAL_HEADER_LINES: u16 = 0;

#[allow(dead_code)]
mod moon {
    use ratatui::style::Color;

    pub const NC: Color = Color::Rgb(31, 29, 48);
    pub const BASE: Color = Color::Rgb(35, 33, 54);
    pub const SURFACE: Color = Color::Rgb(42, 39, 63);
    pub const OVERLAY: Color = Color::Rgb(57, 53, 82);
    pub const MUTED: Color = Color::Rgb(110, 106, 134);
    pub const SUBTLE: Color = Color::Rgb(144, 140, 170);
    pub const TEXT: Color = Color::Rgb(224, 222, 244);
    pub const LOVE: Color = Color::Rgb(235, 111, 146);
    pub const GOLD: Color = Color::Rgb(246, 193, 119);
    pub const ROSE: Color = Color::Rgb(234, 154, 151);
    pub const PINE: Color = Color::Rgb(62, 143, 176);
    pub const FOAM: Color = Color::Rgb(156, 207, 216);
    pub const IRIS: Color = Color::Rgb(196, 167, 231);
    pub const LEAF: Color = Color::Rgb(149, 177, 172);
    pub const HIGHLIGHT_LOW: Color = Color::Rgb(42, 40, 62);
    pub const HIGHLIGHT_MED: Color = Color::Rgb(68, 65, 90);
    pub const HIGHLIGHT_HIGH: Color = Color::Rgb(86, 82, 110);
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
struct Palette {
    nc: Color,
    base: Color,
    surface: Color,
    overlay: Color,
    muted: Color,
    subtle: Color,
    text: Color,
    love: Color,
    gold: Color,
    rose: Color,
    pine: Color,
    foam: Color,
    iris: Color,
    leaf: Color,
    highlight_low: Color,
    highlight_med: Color,
    highlight_high: Color,
}

impl Palette {
    fn from_colorscheme(colorscheme: &ColorSchemeConfig) -> Self {
        Self {
            nc: parse_color(&colorscheme.nc).unwrap_or(moon::NC),
            base: parse_color(&colorscheme.base).unwrap_or(moon::BASE),
            surface: parse_color(&colorscheme.surface).unwrap_or(moon::SURFACE),
            overlay: parse_color(&colorscheme.overlay).unwrap_or(moon::OVERLAY),
            muted: parse_color(&colorscheme.muted).unwrap_or(moon::MUTED),
            subtle: parse_color(&colorscheme.subtle).unwrap_or(moon::SUBTLE),
            text: parse_color(&colorscheme.text).unwrap_or(moon::TEXT),
            love: parse_color(&colorscheme.love).unwrap_or(moon::LOVE),
            gold: parse_color(&colorscheme.gold).unwrap_or(moon::GOLD),
            rose: parse_color(&colorscheme.rose).unwrap_or(moon::ROSE),
            pine: parse_color(&colorscheme.pine).unwrap_or(moon::PINE),
            foam: parse_color(&colorscheme.foam).unwrap_or(moon::FOAM),
            iris: parse_color(&colorscheme.iris).unwrap_or(moon::IRIS),
            leaf: parse_color(&colorscheme.leaf).unwrap_or(moon::LEAF),
            highlight_low: parse_color(&colorscheme.highlight_low).unwrap_or(moon::HIGHLIGHT_LOW),
            highlight_med: parse_color(&colorscheme.highlight_med).unwrap_or(moon::HIGHLIGHT_MED),
            highlight_high: parse_color(&colorscheme.highlight_high)
                .unwrap_or(moon::HIGHLIGHT_HIGH),
        }
    }
}

fn parse_color(input: &str) -> Option<Color> {
    let hex = input.trim().strip_prefix('#').unwrap_or(input.trim());
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }

    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(red, green, blue))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutAreas {
    sidebar: Rect,
    main: Rect,
    footer: Rect,
}

pub fn draw(frame: &mut Frame, app: &App, config: &config::Config) {
    let layout = layout_areas(app, frame.area());
    let palette = Palette::from_colorscheme(&config.colorscheme);

    draw_sidebar(frame, app, layout.sidebar, palette);
    draw_main(frame, app, layout.main, palette);
    draw_footer(frame, app, layout.footer, palette);
}

pub fn selected_terminal_output_area(app: &App, frame_area: Rect) -> Option<(TerminalId, Rect)> {
    let Some(NavItem::Terminal { terminal, .. }) = app.selected_item() else {
        return None;
    };

    let layout = layout_areas(app, frame_area);
    Some((terminal, terminal_output_area(layout.main)))
}

pub fn selected_chat_agent_output_area(app: &App, frame_area: Rect) -> Option<(ChatId, Rect)> {
    let Some(NavItem::Chat { chat, .. }) = app.selected_item() else {
        return None;
    };

    let layout = layout_areas(app, frame_area);
    Some((chat, chat_agent_output_area(layout.main)))
}

fn layout_areas(app: &App, frame_area: Rect) -> LayoutAreas {
    let footer_height = if app.is_prompt_active() { 3 } else { 1 };
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
    output_area_after_header(main, TERMINAL_HEADER_LINES)
}

fn chat_agent_output_area(main: Rect) -> Rect {
    output_area_after_header(main, CHAT_AGENT_HEADER_LINES)
}

fn output_area_after_header(main: Rect, header_lines: u16) -> Rect {
    let inner = pane_inner(main);
    let header_height = header_lines.min(inner.height);

    Rect {
        x: inner.x,
        y: inner.y.saturating_add(header_height),
        width: inner.width,
        height: inner.height.saturating_sub(header_height),
    }
}

fn pane_inner(area: Rect) -> Rect {
    area
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect, palette: Palette) {
    let items = sidebar_items(app, palette);
    let selected = if items.is_empty() {
        None
    } else {
        Some(app.selected.min(items.len() - 1))
    };
    let mut state = ListState::default();
    state.select(selected);

    let focused = focus_is_active(app, FocusMode::Sidebar);
    let list = List::new(items)
        .block(Block::default().style(pane_style(focused, palette)))
        .style(pane_style(focused, palette))
        .highlight_style(
            Style::default()
                .bg(palette.highlight_med)
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, area, &mut state);
}

fn sidebar_items(app: &App, palette: Palette) -> Vec<ListItem<'static>> {
    app.project
        .workspaces
        .iter()
        .flat_map(|workspace| {
            let workspace_line = ListItem::new(Line::from(vec![
                Span::styled("▣ ", Style::default().fg(palette.foam)),
                Span::styled(
                    workspace.name.clone(),
                    Style::default()
                        .fg(palette.text)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            let chat_lines = workspace.chats.iter().map(move |chat| {
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("● ", chat_status_style(chat.status, palette)),
                    Span::raw(chat.name.clone()),
                ]))
            });

            let terminal_lines = workspace.terminals.iter().map(move |terminal| {
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("$ ", terminal_status_style(terminal.status, palette)),
                    Span::raw(terminal_name_label(terminal)),
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

fn draw_main(frame: &mut Frame, app: &App, area: Rect, palette: Palette) {
    let selected_item = app.selected_item();
    let pane_focus = selected_item.and_then(main_pane_focus);
    let focused = pane_focus.is_some_and(|focus| focus_is_active(app, focus));

    let terminal_output_rows = usize::from(terminal_output_area(area).height.max(1));
    let chat_agent_output_rows = usize::from(chat_agent_output_area(area).height.max(1));
    let lines = match selected_item {
        Some(NavItem::Workspace(workspace)) => workspace_details(app, workspace, palette),
        Some(NavItem::Chat { workspace, chat }) => {
            chat_details(app, workspace, chat, chat_agent_output_rows, palette)
        }
        Some(NavItem::Terminal {
            workspace,
            terminal,
        }) => terminal_details(app, workspace, terminal, terminal_output_rows, palette),
        None => vec![Line::from("No workspaces yet.")],
    };

    let paragraph = Paragraph::new(lines)
        .block(Block::default().style(pane_style(focused, palette)))
        .style(pane_style(focused, palette));
    // PTY rows are already laid out to the pane width. Re-wrapping them can
    // turn styled blank rows emitted by nested TUIs into extra visual lines.
    let paragraph = if matches!(selected_item, Some(NavItem::Workspace(_)) | None) {
        paragraph.wrap(Wrap { trim: false })
    } else {
        paragraph
    };
    frame.render_widget(paragraph, area);
}

fn main_pane_focus(item: NavItem) -> Option<FocusMode> {
    match item {
        NavItem::Chat { .. } => Some(FocusMode::Chat),
        NavItem::Terminal { .. } => Some(FocusMode::Terminal),
        NavItem::Workspace(_) => None,
    }
}

fn focus_is_active(app: &App, focus: FocusMode) -> bool {
    !app.is_prompt_active() && app.focus == focus
}

fn pane_style(focused: bool, palette: Palette) -> Style {
    if focused {
        Style::default().fg(palette.text).bg(palette.base)
    } else {
        Style::default().fg(palette.text).bg(palette.nc)
    }
}

fn workspace_details(app: &App, workspace_id: WorkspaceId, palette: Palette) -> Vec<Line<'static>> {
    let Some(workspace) = app.project.workspace(workspace_id) else {
        return vec![Line::from("Missing workspace.")];
    };

    let cwd = workspace
        .cwd
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unset>".to_string());

    let mut lines = vec![
        Line::from(Span::styled(
            workspace.name.clone(),
            Style::default()
                .fg(palette.foam)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("cwd ", Style::default().fg(palette.muted)),
            Span::raw(cwd),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(pad_cell("Chats", 42), Style::default().fg(palette.muted)),
            Span::styled("Terminals", Style::default().fg(palette.muted)),
        ]),
    ];

    let rows = workspace.chats.len().max(workspace.terminals.len());
    if rows == 0 {
        lines.push(Line::from(
            "No chats or terminals. Press `c` or `t` to add one.",
        ));
        return lines;
    }

    for index in 0..rows {
        let mut spans = Vec::new();
        if let Some(chat) = workspace.chats.get(index) {
            spans.push(Span::styled("● ", chat_status_style(chat.status, palette)));
            spans.push(Span::raw(pad_cell(&chat.name, 40)));
        } else {
            spans.push(Span::raw(pad_cell("", 42)));
        }

        if let Some(terminal) = workspace.terminals.get(index) {
            spans.push(Span::styled(
                "$ ",
                terminal_status_style(terminal.status, palette),
            ));
            spans.push(Span::raw(terminal_name_label(terminal)));
        }

        lines.push(Line::from(spans));
    }

    lines
}

fn pad_cell(value: &str, width: usize) -> String {
    let value = if value.chars().count() > width {
        let mut truncated = value
            .chars()
            .take(width.saturating_sub(1))
            .collect::<String>();
        truncated.push('…');
        truncated
    } else {
        value.to_string()
    };
    format!("{value:<width$}")
}

fn chat_details(
    app: &App,
    workspace_id: WorkspaceId,
    chat_id: crate::model::ChatId,
    output_rows: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    if app.project.workspace(workspace_id).is_none() {
        return vec![Line::from("Missing workspace.")];
    }
    let Some(chat) = app.project.chat(workspace_id, chat_id) else {
        return vec![Line::from("Missing chat.")];
    };

    let terminal_id = chat_agent_terminal_id(chat_id);
    let output = app.terminal_render_lines(terminal_id);
    if !app.terminal_output_is_blank(terminal_id) {
        return output
            .into_iter()
            .rev()
            .take(output_rows)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|line| terminal_render_line_to_line(line, palette))
            .collect();
    }

    if matches!(chat.status, ChatStatus::Thinking | ChatStatus::Waiting) {
        return vec![
            Line::from(format!(
                "Pi agent is {}; waiting for output.",
                chat.status.label()
            )),
            Line::from("Press `i` to enter input mode, or `x` to stop."),
        ];
    }

    let mut lines = vec![
        Line::from("Pi agent not started. Press `i` to start and enter input mode."),
        Line::from("Set `pi_agent_command`/`auto_start_pi_agent` in:"),
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
}

fn terminal_details(
    app: &App,
    workspace_id: WorkspaceId,
    terminal_id: crate::model::TerminalId,
    output_rows: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    if app.project.workspace(workspace_id).is_none() {
        return vec![Line::from("Missing workspace.")];
    }
    let Some(terminal) = app.project.terminal(workspace_id, terminal_id) else {
        return vec![Line::from("Missing terminal.")];
    };

    if app.terminal_output_is_blank(terminal_id) {
        let mut lines = vec![match terminal.status {
            TerminalStatus::Running => Line::from("Terminal is running; waiting for output."),
            TerminalStatus::Stopped => Line::from("Terminal is stopped. Press `s` to start it."),
        }];
        lines.push(Line::from(
            "Press `i` to focus PTY input after start, or `x` to stop a running PTY.",
        ));
        if let TerminalLaunch::Command(command) = &terminal.launch {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Command: ", Style::default().fg(palette.muted)),
                Span::raw(command.clone()),
            ]));
        }
        return lines;
    }

    app.terminal_render_lines(terminal_id)
        .into_iter()
        .rev()
        .take(output_rows)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| terminal_render_line_to_line(line, palette))
        .collect()
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect, palette: Palette) {
    if let Some(prompt) = &app.prompt {
        match prompt {
            Prompt::OpenWorkspace(prompt) => draw_text_prompt(
                frame,
                area,
                palette,
                "Path: ",
                &prompt.input,
                prompt.error.as_deref(),
                "enter imports • esc/ctrl-c cancels",
            ),
            Prompt::NewTerminalCommand(prompt) => draw_text_prompt(
                frame,
                area,
                palette,
                "Command: ",
                &prompt.input,
                prompt.error.as_deref(),
                "enter adds command terminal • esc/ctrl-c cancels",
            ),
        }
        return;
    }

    let footer = match app.mode {
        Mode::Normal => Line::styled(FOOTER, Style::default().fg(palette.muted)),
        Mode::Input(_) => Line::styled(
            "input mode • typing goes to selected PTY • Esc returns to normal mode • Ctrl-C sends interrupt",
            Style::default().fg(palette.gold),
        ),
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().bg(palette.base)),
        area,
    );
}

fn draw_text_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    label: &'static str,
    input: &str,
    error: Option<&str>,
    help: &'static str,
) {
    let message = error.unwrap_or(help);
    let message_style = if error.is_some() {
        Style::default().fg(palette.love)
    } else {
        Style::default().fg(palette.muted)
    };
    let prompt = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(label, Style::default().fg(palette.muted)),
            Span::raw(input.to_string()),
            Span::styled("▌", Style::default().fg(palette.gold)),
        ]),
        Line::from(Span::styled(message.to_string(), message_style)),
    ])
    .style(Style::default().fg(palette.text).bg(palette.base));
    frame.render_widget(prompt, area);
}

fn terminal_render_line_to_line(line: TerminalRenderLine, palette: Palette) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|span| Span::styled(span.text, terminal_style(span.style, palette)))
            .collect::<Vec<_>>(),
    )
}

fn terminal_style(style: TerminalCellStyle, palette: Palette) -> Style {
    let mut output = Style::default();
    if let Some(fg) = style.fg {
        output = output.fg(terminal_color(fg, palette));
    }
    if let Some(bg) = style.bg {
        output = output.bg(terminal_color(bg, palette));
    }
    if style.bold {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        output = output.add_modifier(Modifier::ITALIC);
    }
    // Many prompts use underline for decorative path segments. It reads as a
    // selection/cursor artifact inside nested panes, so mult intentionally
    // suppresses underline when rendering embedded PTYs.
    let _ = style.underlined;
    output
}

fn terminal_color(color: TerminalColor, palette: Palette) -> Color {
    match color {
        TerminalColor::Black => palette.base,
        TerminalColor::Red => palette.love,
        TerminalColor::Green => palette.leaf,
        TerminalColor::Yellow => palette.gold,
        TerminalColor::Blue => palette.pine,
        TerminalColor::Magenta => palette.iris,
        TerminalColor::Cyan => palette.foam,
        TerminalColor::White => palette.text,
        TerminalColor::BrightBlack => palette.muted,
        TerminalColor::BrightRed => palette.love,
        TerminalColor::BrightGreen => palette.leaf,
        TerminalColor::BrightYellow => palette.gold,
        TerminalColor::BrightBlue => palette.pine,
        TerminalColor::BrightMagenta => palette.iris,
        TerminalColor::BrightCyan => palette.foam,
        TerminalColor::BrightWhite => palette.text,
        TerminalColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn terminal_name_label(terminal: &TerminalSession) -> String {
    match &terminal.launch {
        TerminalLaunch::Command(command) if terminal.name.starts_with("cmd: ") => {
            let command = command.trim();
            if command.is_empty() {
                terminal
                    .name
                    .strip_prefix("cmd: ")
                    .unwrap_or(&terminal.name)
                    .to_string()
            } else {
                command.to_string()
            }
        }
        _ => terminal.name.clone(),
    }
}

fn chat_status_style(status: ChatStatus, palette: Palette) -> Style {
    let color = match status {
        ChatStatus::Failed => palette.love,
        ChatStatus::Waiting => palette.gold,
        ChatStatus::Thinking => palette.pine,
        ChatStatus::Idle | ChatStatus::Done => palette.muted,
    };

    Style::default().fg(color)
}

fn terminal_status_style(status: TerminalStatus, palette: Palette) -> Style {
    let color = match status {
        TerminalStatus::Running => palette.pine,
        TerminalStatus::Stopped => palette.muted,
    };

    Style::default().fg(color)
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    #[test]
    fn draw_handles_empty_workspace_list() {
        let mut app = App::default();
        app.project.workspaces.clear();
        app.selected = 0;
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| draw(frame, &app, &config::Config::default()))
            .expect("draw app");
    }

    #[test]
    fn terminal_name_label_hides_legacy_command_prefix() {
        let terminal = TerminalSession {
            id: TerminalId(99),
            name: "cmd: ping".to_string(),
            status: TerminalStatus::Running,
            launch: TerminalLaunch::Command("ping example.com".to_string()),
        };

        assert_eq!(terminal_name_label(&terminal), "ping example.com");
    }

    #[test]
    fn terminal_output_does_not_wrap_styled_blank_rows() {
        let mut app = App::default();
        let nav_items = app.nav_items();
        let (selected, terminal_id) = nav_items
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, *terminal)),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.selected = selected;

        let frame_area = Rect::new(0, 0, 50, 6);
        let (_, output_area) = selected_terminal_output_area(&app, frame_area)
            .expect("terminal selection has output area");
        app.resize_terminal_buffer(terminal_id, output_area.height, output_area.width);
        let spaces = " ".repeat(usize::from(output_area.width));
        app.append_terminal_output(terminal_id, &format!("\x1b[44m{spaces}\x1b[0m\r\nnext"));

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, &app, &config::Config::default()))
            .expect("draw app");

        assert_eq!(
            buffer_text(terminal.backend(), output_area.x, output_area.y + 1, 4),
            "next"
        );
    }

    fn buffer_text(backend: &TestBackend, x: u16, y: u16, width: u16) -> String {
        let mut text = String::new();
        for offset in 0..width {
            text.push_str(
                backend
                    .buffer()
                    .cell((x + offset, y))
                    .expect("cell is in bounds")
                    .symbol(),
            );
        }
        text
    }

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

        assert_eq!(area.width, 86);
        assert_eq!(area.height, 39);
    }

    #[test]
    fn selected_terminal_output_area_is_absent_for_non_terminal_selection() {
        let app = App::default();

        assert_eq!(
            selected_terminal_output_area(&app, Rect::new(0, 0, 120, 40)),
            None
        );
    }

    #[test]
    fn selected_chat_agent_output_area_tracks_visible_main_pane_size() {
        let mut app = App::default();
        app.selected = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Chat { .. }))
            .expect("seed state has a chat");

        let (_, area) = selected_chat_agent_output_area(&app, Rect::new(0, 0, 120, 40))
            .expect("chat selection has pi output area");

        assert_eq!(area.width, 86);
        assert_eq!(area.height, 39);
    }
}
