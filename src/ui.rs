use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::{
    app::{
        chat_agent_terminal_id, App, CommandPaletteEntry, FocusMode, Mode, NavItem, Prompt,
        SearchScope,
    },
    config::{self, ColorSchemeConfig},
    model::{
        ChatId, ChatStatus, TerminalId, TerminalLaunch, TerminalSession, TerminalStatus,
        WorkspaceId,
    },
    pty::PtyRuntime,
};

const FOOTER: &str = "j/k nav/scroll • mouse wheel scroll • shift-drag select • : commands • / search • enter pane • esc sidebar • n a agent • n t terminal • n c command • n w workspace • d d delete • i input • q quit";
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
struct PaneLayout {
    sidebar_width: u16,
    min_main_width: u16,
    footer_height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutAreas {
    sidebar: Rect,
    main: Rect,
    footer: Rect,
}

#[derive(Debug, Clone, Copy)]
struct PaneRenderStyle {
    focused: bool,
    palette: Palette,
}

pub fn draw(frame: &mut Frame, app: &App, pty_runtime: &PtyRuntime, config: &config::Config) {
    let layout = layout_areas(app, frame.area());
    let palette = Palette::from_colorscheme(&config.colorscheme);

    draw_sidebar(frame, app, layout.sidebar, palette);
    draw_main(frame, app, pty_runtime, layout.main, palette);
    draw_footer(frame, app, pty_runtime, layout.footer, palette);
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
    PaneLayout::for_app(app).areas(frame_area)
}

impl PaneLayout {
    fn for_app(app: &App) -> Self {
        Self {
            sidebar_width: 34,
            min_main_width: 40,
            footer_height: footer_height(app),
        }
    }

    fn areas(self, frame_area: Rect) -> LayoutAreas {
        let [body, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(self.footer_height)])
                .areas(frame_area);
        let [sidebar, main] = Layout::horizontal([
            Constraint::Length(self.sidebar_width),
            Constraint::Min(self.min_main_width),
        ])
        .areas(body);

        LayoutAreas {
            sidebar,
            main,
            footer,
        }
    }
}

fn footer_height(app: &App) -> u16 {
    match &app.prompt {
        Some(Prompt::CommandPalette(_)) => 7,
        Some(_) => 3,
        None => 1,
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
    let style = pane_style(focused, palette);
    frame.render_widget(Block::default().style(style), area);

    let list = List::new(items)
        .style(style)
        .highlight_style(
            Style::default()
                .bg(palette.highlight_med)
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ");

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

fn draw_main(frame: &mut Frame, app: &App, pty_runtime: &PtyRuntime, area: Rect, palette: Palette) {
    let selected_item = app.selected_item();
    let pane_focus = selected_item.and_then(main_pane_focus);
    let focused = pane_focus.is_some_and(|focus| focus_is_active(app, focus));

    match selected_item {
        Some(NavItem::Workspace(workspace)) => render_lines_pane(
            frame,
            area,
            workspace_details(app, workspace, palette),
            focused,
            palette,
            true,
        ),
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
        None => render_lines_pane(
            frame,
            area,
            vec![Line::from("No workspaces yet.")],
            focused,
            palette,
            true,
        ),
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
            "No chats or terminals. Press `n a`, `n t`, or `n c` to add one.",
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
    let output_rows = usize::from(chat_agent_output_area(area).height.max(1));
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
        render_lines_pane(
            frame,
            area,
            search_result_lines("chat transcript", query, lines, output_rows, palette),
            focused,
            palette,
            false,
        );
        return;
    }

    let terminal_id = chat_agent_terminal_id(chat_id);
    if !pty_runtime.terminal_output_is_blank(terminal_id) {
        if let Some(parser) = pty_runtime.parser(terminal_id) {
            render_terminal_parser(frame, area, parser, focused, palette);
            return;
        }
    }

    let lines = if matches!(chat.status, ChatStatus::Thinking | ChatStatus::Waiting) {
        vec![
            Line::from(format!(
                "Pi agent is {}; waiting for output.",
                chat.status.label()
            )),
            Line::from("Press `i` to enter input mode."),
        ]
    } else {
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
    let output_rows = usize::from(terminal_output_area(area).height.max(1));
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

    if let Some(lines) =
        app.terminal_search_matches(terminal_id, pty_runtime.terminal_all_lines(terminal_id))
    {
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

    if pty_runtime.terminal_output_is_blank(terminal_id) {
        let mut lines = vec![match terminal.status {
            TerminalStatus::Running => {
                Line::from("Terminal is running; waiting for output. Press `i` to focus PTY input.")
            }
            TerminalStatus::Stopped => {
                Line::from("Terminal is stopped. Press `i` to start and enter input mode.")
            }
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

    if let Some(parser) = pty_runtime.parser(terminal_id) {
        render_terminal_parser(frame, area, parser, focused, palette);
    }
}

fn render_terminal_parser(
    frame: &mut Frame,
    area: Rect,
    parser: &vt100::Parser,
    focused: bool,
    palette: Palette,
) {
    let cursor_style = Style::default().fg(palette.nc).bg(palette.text);
    let cursor = Cursor::default()
        .symbol("█")
        .style(cursor_style)
        .visibility(parser.screen().scrollback() == 0);
    let pseudo_term = PseudoTerminal::new(parser.screen())
        .block(Block::default().style(pane_style(focused, palette)))
        .cursor(cursor);
    frame.render_widget(pseudo_term, area);
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

fn draw_footer(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    area: Rect,
    palette: Palette,
) {
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
            Prompt::CommandPalette(prompt) => draw_command_palette_prompt(
                frame,
                area,
                palette,
                &prompt.input,
                prompt.selected,
                app.active_command_palette_entries(),
            ),
            Prompt::Search(prompt) => draw_text_prompt(
                frame,
                area,
                palette,
                search_prompt_label(prompt.scope),
                &prompt.input,
                prompt.error.as_deref(),
                "enter applies filter • empty enter clears • esc/ctrl-c cancels",
            ),
        }
        return;
    }

    let footer = match app.mode {
        Mode::Normal => normal_footer(app, pty_runtime, palette),
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

fn normal_footer(app: &App, pty_runtime: &PtyRuntime, palette: Palette) -> Line<'static> {
    if let Some(status) = search_status(app, pty_runtime) {
        Line::from(vec![
            Span::styled(FOOTER, Style::default().fg(palette.muted)),
            Span::styled(" • ", Style::default().fg(palette.muted)),
            Span::styled(status, Style::default().fg(palette.gold)),
        ])
    } else {
        Line::styled(FOOTER, Style::default().fg(palette.muted))
    }
}

fn search_status(app: &App, pty_runtime: &PtyRuntime) -> Option<String> {
    let search = app.active_search.as_ref()?;
    match search.scope {
        SearchScope::Chat(_) => app.chat_search_status(),
        SearchScope::Terminal(terminal) => {
            app.terminal_search_status(terminal, pty_runtime.terminal_all_lines(terminal))
        }
    }
}

fn search_prompt_label(scope: SearchScope) -> &'static str {
    match scope {
        SearchScope::Terminal(_) => "Search terminal: ",
        SearchScope::Chat(_) => "Search chat: ",
    }
}

fn draw_command_palette_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    input: &str,
    selected: usize,
    entries: Vec<CommandPaletteEntry>,
) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Command: ", Style::default().fg(palette.muted)),
            Span::raw(input.to_string()),
            Span::styled("▌", Style::default().fg(palette.gold)),
        ]),
        Line::from(Span::styled(
            "type to filter • ↑/↓ select • enter runs • esc cancels".to_string(),
            Style::default().fg(palette.muted),
        )),
    ];

    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "No matching commands".to_string(),
            Style::default().fg(palette.love),
        )));
    } else {
        let max_entries = usize::from(area.height.saturating_sub(2)).max(1);
        let start = selected.saturating_sub(max_entries.saturating_sub(1));
        lines.extend(
            entries
                .into_iter()
                .enumerate()
                .skip(start)
                .take(max_entries)
                .map(|(index, entry)| command_palette_line(entry, index == selected, palette)),
        );
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(palette.text).bg(palette.base)),
        area,
    );
}

fn command_palette_line(
    entry: CommandPaletteEntry,
    selected: bool,
    palette: Palette,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let style = if selected {
        Style::default()
            .fg(palette.text)
            .bg(palette.highlight_med)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text)
    };

    Line::from(vec![
        Span::styled(marker, style),
        Span::styled(entry.label, style),
        Span::styled(" — ", Style::default().fg(palette.muted)),
        Span::styled(entry.help, Style::default().fg(palette.muted)),
    ])
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
        let pty_runtime = PtyRuntime::new_offline();

        terminal
            .draw(|frame| draw(frame, &app, &pty_runtime, &config::Config::default()))
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
    fn empty_workspace_hint_matches_current_prefix_controls() {
        let mut app = App::default();
        app.project.workspaces[0].chats.clear();
        app.project.workspaces[0].terminals.clear();
        let workspace = app.project.workspaces[0].id;

        let text = lines_text(workspace_details(&app, workspace, test_palette()));

        assert!(text.contains("Press `n a`, `n t`, or `n c`"));
        assert!(!text.contains("Press `c` or `t`"));
    }

    #[test]
    fn blank_chat_hint_only_mentions_implemented_input_key() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.project.workspaces[0].chats[0].status = ChatStatus::Waiting;

        app.selected = app
            .nav_items()
            .iter()
            .position(|item| *item == NavItem::Chat { workspace, chat })
            .expect("chat exists");
        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);

        assert!(text.contains("Press `i` to enter input mode."));
        assert!(!text.contains("`x`"));
    }

    #[test]
    fn blank_terminal_hint_only_mentions_implemented_start_key() {
        let app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;

        let mut app = app;
        app.selected = app
            .nav_items()
            .iter()
            .position(|item| {
                *item
                    == NavItem::Terminal {
                        workspace,
                        terminal,
                    }
            })
            .expect("terminal exists");
        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);

        assert!(text.contains("Press `i` to start and enter input mode."));
        assert!(!text.contains("Press `s`"));
        assert!(!text.contains("`x`"));
    }

    #[test]
    fn scrolled_terminal_output_hides_cursor() {
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                NavItem::Terminal { terminal, .. } => Some((index, *terminal)),
                _ => None,
            })
            .expect("seed state has a terminal");
        app.selected = selected;
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, crate::pty::PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree");
        assert!(pty_runtime.scroll_up(terminal_id, 1).expect("scroll up"));

        let text = draw_text(&app, &pty_runtime, 50, 6);

        assert!(!text.contains('█'));
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
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(
                terminal_id,
                crate::pty::PtyDimensions {
                    rows: output_area.height,
                    cols: output_area.width,
                },
            )
            .expect("resize parser");
        let spaces = " ".repeat(usize::from(output_area.width));
        pty_runtime.process_terminal_output(
            terminal_id,
            format!("\x1b[44m{spaces}\x1b[0m\r\nnext").as_bytes(),
        );

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, &app, &pty_runtime, &config::Config::default()))
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

    fn draw_text(app: &App, pty_runtime: &PtyRuntime, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, app, pty_runtime, &config::Config::default()))
            .expect("draw app");
        format!("{:?}", terminal.backend().buffer())
    }

    fn lines_text(lines: Vec<Line<'_>>) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn test_palette() -> Palette {
        Palette::from_colorscheme(&config::Config::default().colorscheme)
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
