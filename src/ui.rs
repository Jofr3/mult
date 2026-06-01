use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use tui_term::widget::{
    Cell as TerminalCellWidget, Cursor, PseudoTerminal, Screen as TerminalScreenWidget,
};

use crate::{
    app::{
        App, CommandPaletteEntry, FocusMode, NavItem, OpenWorkspaceMatch, OpenWorkspaceMode,
        Prompt, SearchScope, TextSelection,
    },
    config::{self, ColorSchemeConfig},
    model::{
        ChatId, ChatStatus, PtyKey, TerminalId, TerminalLaunch, TerminalSession, TerminalStatus,
        Workspace, WorkspaceId,
    },
    pty::PtyRuntime,
};

const CHAT_AGENT_HEADER_LINES: u16 = 0;
const TERMINAL_HEADER_LINES: u16 = 0;
const SIDEBAR_SELECTION_SYMBOL: &str = " ";
const WORKSPACE_ICON: &str = "▣ ";
const GIT_BRANCH_ICON: &str = "";

mod moon {
    use ratatui::style::Color;

    pub const NC: Color = Color::Rgb(31, 29, 48);
    pub const BASE: Color = Color::Rgb(35, 33, 54);
    pub const MUTED: Color = Color::Rgb(110, 106, 134);
    pub const TEXT: Color = Color::Rgb(224, 222, 244);
    pub const LOVE: Color = Color::Rgb(235, 111, 146);
    pub const GOLD: Color = Color::Rgb(246, 193, 119);
    pub const PINE: Color = Color::Rgb(62, 143, 176);
    pub const FOAM: Color = Color::Rgb(156, 207, 216);
    pub const IRIS: Color = Color::Rgb(196, 167, 231);
    pub const HIGHLIGHT_MED: Color = Color::Rgb(68, 65, 90);
}

#[derive(Debug, Clone, Copy)]
struct Palette {
    nc: Color,
    base: Color,
    muted: Color,
    text: Color,
    love: Color,
    gold: Color,
    pine: Color,
    foam: Color,
    iris: Color,
    highlight_med: Color,
    cursor: Color,
    success: Color,
}

impl Palette {
    fn from_colorscheme(colorscheme: &ColorSchemeConfig) -> Self {
        Self {
            nc: parse_color(&colorscheme.nc).unwrap_or(moon::NC),
            base: parse_color(&colorscheme.base).unwrap_or(moon::BASE),
            muted: parse_color(&colorscheme.muted).unwrap_or(moon::MUTED),
            text: parse_color(&colorscheme.text).unwrap_or(moon::TEXT),
            love: parse_color(&colorscheme.love).unwrap_or(moon::LOVE),
            gold: parse_color(&colorscheme.gold).unwrap_or(moon::GOLD),
            pine: parse_color(&colorscheme.pine).unwrap_or(moon::PINE),
            foam: parse_color(&colorscheme.foam).unwrap_or(moon::FOAM),
            iris: parse_color(&colorscheme.iris).unwrap_or(moon::IRIS),
            highlight_med: parse_color(&colorscheme.highlight_med).unwrap_or(moon::HIGHLIGHT_MED),
            cursor: parse_color(&colorscheme.cursor).unwrap_or(Color::Rgb(255, 255, 255)),
            success: parse_color(&colorscheme.success).unwrap_or(Color::Rgb(62, 143, 84)),
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

/// Relative luminance per WCAG 2.x (sRGB). Non-RGB colors are treated as dark.
fn relative_luminance(color: Color) -> f64 {
    let Color::Rgb(red, green, blue) = color else {
        return 0.0;
    };
    fn linearize(channel: u8) -> f64 {
        let channel = f64::from(channel) / 255.0;
        if channel <= 0.039_28 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linearize(red) + 0.7152 * linearize(green) + 0.0722 * linearize(blue)
}

/// WCAG contrast ratio between two colors (1.0 = identical, up to 21.0).
fn contrast_ratio(a: Color, b: Color) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (high, low) = if la >= lb { (la, lb) } else { (lb, la) };
    (high + 0.05) / (low + 0.05)
}

/// Foreground for text drawn on `background`: keep `preferred` while it stays
/// legible there, otherwise fall back to black or white. This preserves the
/// default (dark) theme's exact look while staying readable on light or
/// inverted user palettes, where a fixed dark foreground would wash out.
fn readable_fg(preferred: Color, background: Color) -> Color {
    const MIN_CONTRAST: f64 = 4.5; // WCAG AA for normal-size text
    if contrast_ratio(preferred, background) >= MIN_CONTRAST {
        preferred
    } else if relative_luminance(background) > 0.179 {
        Color::Rgb(0, 0, 0)
    } else {
        Color::Rgb(255, 255, 255)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneLayout {
    sidebar_width: u16,
    min_main_width: u16,
    prompt_height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutAreas {
    sidebar: Rect,
    main: Rect,
    prompt: Rect,
}

#[derive(Debug, Clone, Copy)]
struct PaneRenderStyle {
    focused: bool,
    palette: Palette,
}

pub fn draw(frame: &mut Frame, app: &App, pty_runtime: &PtyRuntime, config: &config::Config) {
    let layout = layout_areas(app, frame.area());
    let palette = Palette::from_colorscheme(&config.colorscheme);

    draw_sidebar(frame, app, pty_runtime, layout.sidebar, palette);
    draw_main(frame, app, pty_runtime, layout.main, palette);
    draw_prompt_area(frame, app, layout.prompt, palette, config);
}

pub fn selected_terminal_output_area(app: &App, frame_area: Rect) -> Option<(TerminalId, Rect)> {
    let Some(NavItem::Terminal { terminal, .. }) = app.selected_item() else {
        return None;
    };

    Some((terminal, terminal_output_area_for(app, frame_area)))
}

pub fn selected_chat_agent_output_area(app: &App, frame_area: Rect) -> Option<(ChatId, Rect)> {
    let Some(NavItem::Chat { chat, .. }) = app.selected_item() else {
        return None;
    };

    Some((chat, chat_agent_output_area_for(app, frame_area)))
}

pub fn terminal_output_area_for(app: &App, frame_area: Rect) -> Rect {
    let layout = layout_areas(app, frame_area);
    terminal_output_area(layout.main)
}

pub fn chat_agent_output_area_for(app: &App, frame_area: Rect) -> Rect {
    let layout = layout_areas(app, frame_area);
    chat_agent_output_area(layout.main)
}

fn layout_areas(app: &App, frame_area: Rect) -> LayoutAreas {
    PaneLayout::for_app(app).areas(frame_area)
}

impl PaneLayout {
    fn for_app(app: &App) -> Self {
        Self {
            sidebar_width: 34,
            min_main_width: 40,
            prompt_height: prompt_height(app),
        }
    }

    fn areas(self, frame_area: Rect) -> LayoutAreas {
        let [body, prompt] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(self.prompt_height)])
                .areas(frame_area);
        let [sidebar, main] = Layout::horizontal([
            Constraint::Length(self.sidebar_width),
            Constraint::Min(self.min_main_width),
        ])
        .areas(body);

        LayoutAreas {
            sidebar,
            main,
            prompt,
        }
    }
}

fn prompt_height(app: &App) -> u16 {
    match &app.prompt {
        Some(Prompt::CommandPalette(_)) => 7,
        Some(Prompt::OpenWorkspace(prompt))
            if prompt.mode == OpenWorkspaceMode::ConfiguredProjects =>
        {
            7
        }
        Some(_) => 3,
        None => 0,
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

fn draw_sidebar(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    area: Rect,
    palette: Palette,
) {
    let items = sidebar_items(app, pty_runtime, palette, sidebar_item_width(area));
    let selected = sidebar_selected_index(app, items.len());
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
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(SIDEBAR_SELECTION_SYMBOL)
        .highlight_spacing(HighlightSpacing::Always);

    frame.render_stateful_widget(list, area, &mut state);
}

fn sidebar_items(
    app: &App,
    pty_runtime: &PtyRuntime,
    palette: Palette,
    item_width: usize,
) -> Vec<ListItem<'static>> {
    let mut items = Vec::new();

    for (workspace_index, workspace) in app.project.workspaces.iter().enumerate() {
        if workspace_index > 0 {
            items.push(ListItem::new(Line::from("")));
        }

        items.push(ListItem::new(workspace_sidebar_line(
            workspace,
            app.workspace_git_branch(workspace.id),
            palette,
            item_width,
        )));

        items.extend(workspace.chats.iter().map(|chat| {
            let done_seen = app.chat_done_seen(chat.id);
            ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled("● ", chat_status_style(chat.status, done_seen, palette)),
                Span::raw(chat.name.clone()),
            ]))
        }));

        items.extend(workspace.terminals.iter().map(|terminal| {
            let focused = terminal_sidebar_item_is_focused(app, workspace.id, terminal.id);
            ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "$ ",
                    terminal_icon_style(terminal, pty_runtime, focused, palette),
                ),
                Span::raw(terminal_display_label(
                    terminal,
                    pty_runtime,
                    item_width.saturating_sub(4),
                )),
            ]))
        }));
    }

    items
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

fn sidebar_selected_index(app: &App, item_count: usize) -> Option<usize> {
    if item_count == 0 || app.nav_len() == 0 {
        return None;
    }

    let target_nav_index = app.selected_index().unwrap_or(0);
    let mut nav_index = 0;
    let mut item_index = 0;

    for (workspace_index, workspace) in app.project.workspaces.iter().enumerate() {
        if workspace_index > 0 {
            item_index += 1;
        }
        item_index += 1;

        for _ in &workspace.chats {
            if nav_index == target_nav_index {
                return Some(item_index.min(item_count - 1));
            }
            nav_index += 1;
            item_index += 1;
        }

        for _ in &workspace.terminals {
            if nav_index == target_nav_index {
                return Some(item_index.min(item_count - 1));
            }
            nav_index += 1;
            item_index += 1;
        }
    }

    None
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

fn text_width(value: &str) -> usize {
    Span::raw(value).width()
}

fn truncate_text(value: &str, max_width: usize) -> String {
    if text_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let ellipsis_width = text_width("…");
    let mut output = String::new();
    let mut width = 0;
    for ch in value.chars() {
        let ch = ch.to_string();
        let ch_width = text_width(&ch);
        if width + ch_width + ellipsis_width > max_width {
            break;
        }
        output.push_str(&ch);
        width += ch_width;
    }
    output.push('…');
    output
}

fn draw_main(frame: &mut Frame, app: &App, pty_runtime: &PtyRuntime, area: Rect, palette: Palette) {
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

    let lines = if matches!(chat.status, ChatStatus::Thinking | ChatStatus::Waiting) {
        vec![
            Line::from(format!(
                "Pi agent is {}; waiting for output.",
                chat.status.label()
            )),
            Line::from("Type to send input to the selected agent PTY."),
        ]
    } else {
        let mut lines = vec![
            Line::from("Pi agent not started. Type to start it and send input."),
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

    if let Some(lines) = app.terminal_search_matches(
        terminal_id,
        pty_runtime.terminal_all_lines(PtyKey::Terminal(terminal_id)),
    ) {
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
        let mut lines = vec![match terminal.status {
            TerminalStatus::Running => {
                Line::from("Terminal is running; waiting for output. Type to send PTY input.")
            }
            TerminalStatus::Stopped => {
                Line::from("Terminal is stopped. Type to start it and send input.")
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

fn render_terminal_parser(
    frame: &mut Frame,
    area: Rect,
    parser: &vt100::Parser,
    focused: bool,
    palette: Palette,
    selection: Option<&TextSelection>,
) {
    let cursor_style = Style::default().fg(palette.cursor).bg(palette.base);
    let cursor_overlay_style = Style::default()
        .fg(readable_fg(palette.nc, palette.cursor))
        .bg(palette.cursor);
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

#[derive(Debug)]
struct TerminalScreen {
    rows: u16,
    cols: u16,
    cursor_position: (u16, u16),
    hide_cursor: bool,
    cells: Vec<TerminalCell>,
}

#[derive(Debug, Clone)]
struct TerminalCell {
    symbol: String,
    has_contents: bool,
    style: Style,
}

impl TerminalScreen {
    fn from_vt100(screen: &vt100::Screen) -> Self {
        let (rows, cols) = screen.size();
        let cells = (0..rows)
            .flat_map(|row| {
                (0..cols).map(move |col| {
                    screen
                        .cell(row, col)
                        .map(TerminalCell::from_vt100)
                        .unwrap_or_default()
                })
            })
            .collect();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let scrollback = u16::try_from(screen.scrollback()).unwrap_or(u16::MAX);

        Self {
            rows,
            cols,
            cursor_position: (cursor_row.saturating_add(scrollback), cursor_col),
            hide_cursor: screen.hide_cursor(),
            cells,
        }
    }

    fn cell_index(&self, row: u16, col: u16) -> Option<usize> {
        if row >= self.rows || col >= self.cols {
            return None;
        }

        Some(usize::from(row) * usize::from(self.cols) + usize::from(col))
    }
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            has_contents: false,
            style: Style::reset(),
        }
    }
}

impl TerminalCell {
    fn from_vt100(cell: &vt100::Cell) -> Self {
        let mut modifier = Modifier::empty();
        if cell.bold() {
            modifier |= Modifier::BOLD;
        }
        if cell.italic() {
            modifier |= Modifier::ITALIC;
        }
        if cell.underline() {
            modifier |= Modifier::UNDERLINED;
        }
        if cell.inverse() {
            modifier |= Modifier::REVERSED;
        }

        Self {
            symbol: cell.contents().to_string(),
            has_contents: cell.has_contents(),
            style: Style::reset()
                .fg(vt100_color_to_ratatui(cell.fgcolor()))
                .bg(vt100_color_to_ratatui(cell.bgcolor()))
                .add_modifier(modifier),
        }
    }
}

impl TerminalScreenWidget for TerminalScreen {
    type C = TerminalCell;

    fn cell(&self, row: u16, col: u16) -> Option<&Self::C> {
        self.cell_index(row, col)
            .and_then(|index| self.cells.get(index))
    }

    fn hide_cursor(&self) -> bool {
        self.hide_cursor
    }

    fn cursor_position(&self) -> (u16, u16) {
        self.cursor_position
    }
}

impl TerminalCellWidget for TerminalCell {
    fn has_contents(&self) -> bool {
        self.has_contents
    }

    fn apply(&self, cell: &mut ratatui::buffer::Cell) {
        if self.has_contents {
            cell.set_symbol(&self.symbol);
        }
        cell.set_style(self.style);
    }
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn render_text_selection(
    frame: &mut Frame,
    area: Rect,
    row_content_widths: &[u16],
    selection: &TextSelection,
    palette: Palette,
) {
    let terminal_rows = u16::try_from(row_content_widths.len()).unwrap_or(u16::MAX);
    if area.is_empty() || terminal_rows == 0 {
        return;
    }

    let visible_rows = area.height.min(terminal_rows);
    let visible_last_row = i32::from(visible_rows.saturating_sub(1));
    let range = selection.normalized_range();
    if range.end.row < 0 || range.start.row > visible_last_row {
        return;
    }

    let start_row = range.start.row.max(0);
    let end_row = range.end.row.min(visible_last_row);
    let start_col = range.start.col.min(area.width.saturating_sub(1));
    let end_col = range.end.col.min(area.width.saturating_sub(1));
    let style = Style::default()
        .fg(readable_fg(palette.nc, palette.foam))
        .bg(palette.foam);

    for row in start_row..=end_row {
        // Clip the highlight to the row's glyph extent so trailing blank cells
        // of short lines are not painted as selected — matching the text that
        // `contents_between` actually copies. A fully blank row highlights
        // nothing.
        let content_width = usize::try_from(row)
            .ok()
            .and_then(|index| row_content_widths.get(index))
            .copied()
            .unwrap_or(0);
        if content_width == 0 {
            continue;
        }
        let content_last_col = content_width.saturating_sub(1);
        let row_start_col = if row == range.start.row { start_col } else { 0 };
        let row_end_col = if row == range.end.row {
            end_col
        } else {
            area.width.saturating_sub(1)
        }
        .min(content_last_col);
        if row_start_col > row_end_col {
            continue;
        }
        let row = u16::try_from(row).unwrap_or(0);
        frame.buffer_mut().set_style(
            Rect::new(
                area.x.saturating_add(row_start_col),
                area.y.saturating_add(row),
                row_end_col.saturating_sub(row_start_col).saturating_add(1),
                1,
            ),
            style,
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

fn draw_prompt_area(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    palette: Palette,
    config: &config::Config,
) {
    let Some(prompt) = &app.prompt else {
        return;
    };

    match prompt {
        Prompt::OpenWorkspace(prompt) if prompt.mode == OpenWorkspaceMode::ConfiguredProjects => {
            draw_open_workspace_prompt(
                frame,
                area,
                palette,
                &prompt.input,
                prompt.selected,
                prompt.error.as_deref(),
                app.open_workspace_matches(&config.projects),
            )
        }
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
}

fn search_prompt_label(scope: SearchScope) -> &'static str {
    match scope {
        SearchScope::Terminal(_) => "Search terminal: ",
        SearchScope::Chat(_) => "Search chat: ",
    }
}

fn draw_open_workspace_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    input: &str,
    selected: usize,
    error: Option<&str>,
    entries: Vec<OpenWorkspaceMatch>,
) {
    let mut lines = vec![Line::from(vec![
        Span::styled("Project: ", Style::default().fg(palette.muted)),
        Span::raw(input.to_string()),
        Span::styled("▌", Style::default().fg(palette.cursor)),
    ])];

    if let Some(error) = error {
        lines.push(Line::from(Span::styled(
            error.to_string(),
            Style::default().fg(palette.love),
        )));
    }

    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "No matching configured projects".to_string(),
            Style::default().fg(palette.love),
        )));
    } else {
        let max_entries = usize::from(area.height.saturating_sub(lines.len() as u16)).max(1);
        let start = selected.saturating_sub(max_entries.saturating_sub(1));
        lines.extend(
            entries
                .into_iter()
                .enumerate()
                .skip(start)
                .take(max_entries)
                .map(|(index, entry)| open_workspace_match_line(entry, index == selected, palette)),
        );
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(palette.text).bg(palette.base)),
        area,
    );
}

fn open_workspace_match_line(
    entry: OpenWorkspaceMatch,
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
    let path = entry.path.display().to_string();

    Line::from(vec![
        Span::styled(marker, style),
        Span::styled(entry.name, style),
        Span::styled(" ", Style::default().fg(palette.muted)),
        Span::styled(path, Style::default().fg(palette.muted)),
    ])
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
            Span::styled("▌", Style::default().fg(palette.cursor)),
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
            Span::styled("▌", Style::default().fg(palette.cursor)),
        ]),
        Line::from(Span::styled(message.to_string(), message_style)),
    ])
    .style(Style::default().fg(palette.text).bg(palette.base));
    frame.render_widget(prompt, area);
}

fn terminal_display_label(
    terminal: &TerminalSession,
    pty_runtime: &PtyRuntime,
    max_width: usize,
) -> String {
    truncate_text(&terminal_command_label(terminal, pty_runtime), max_width)
}

fn terminal_command_label(terminal: &TerminalSession, pty_runtime: &PtyRuntime) -> String {
    match &terminal.launch {
        TerminalLaunch::Command(command) => command_label_or_default(command),
        TerminalLaunch::Shell => pty_runtime
            .terminal_last_command(PtyKey::Terminal(terminal.id))
            .map(command_label_or_default)
            .unwrap_or_else(|| "terminal".to_string()),
    }
}

fn command_label_or_default(command: &str) -> String {
    let command = command.trim();
    if command.is_empty() || command == "clear" {
        "terminal".to_string()
    } else {
        command.to_string()
    }
}

/// Color of the agent status dot in the sidebar. Blue (`pine`, running) and
/// gray (`muted`, inactive) are live states; green (`success`), yellow
/// (`gold`), and red (`love`) act as notifications that the agent wants the
/// user's attention. Green is suppressed once the finished agent has been seen
/// (`done_seen`); yellow and red persist until the status itself changes — i.e.
/// until a new prompt or an answered option moves the agent back to running.
fn chat_status_style(status: ChatStatus, done_seen: bool, palette: Palette) -> Style {
    let color = match status {
        ChatStatus::Thinking => palette.pine,
        ChatStatus::Waiting => palette.gold,
        ChatStatus::Failed => palette.love,
        ChatStatus::Done if !done_seen => palette.success,
        ChatStatus::Done | ChatStatus::Idle => palette.muted,
    };

    Style::default().fg(color)
}

fn terminal_icon_style(
    terminal: &TerminalSession,
    pty_runtime: &PtyRuntime,
    focused: bool,
    palette: Palette,
) -> Style {
    let color = if terminal_has_active_command(terminal, pty_runtime) {
        palette.pine
    } else if let Some(exit) = pty_runtime.terminal_exit_status(PtyKey::Terminal(terminal.id)) {
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

fn terminal_has_active_command(terminal: &TerminalSession, pty_runtime: &PtyRuntime) -> bool {
    matches!(terminal.launch, TerminalLaunch::Command(_))
        && pty_runtime.is_running(PtyKey::Terminal(terminal.id))
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::app::SelectionCell;

    #[test]
    fn draw_handles_empty_workspace_list() {
        let mut app = App::default();
        app.project.workspaces.clear();
        app.select_nav_index(0);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        let pty_runtime = PtyRuntime::new_offline();

        terminal
            .draw(|frame| draw(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");
    }

    #[test]
    fn terminal_display_label_uses_command_or_default_and_truncates() {
        let pty_runtime = PtyRuntime::new_offline();
        let command_terminal = TerminalSession {
            id: TerminalId(99),
            name: "cmd: ping".to_string(),
            status: TerminalStatus::Running,
            launch: TerminalLaunch::Command("ping example.com".to_string()),
        };
        let shell_terminal = TerminalSession {
            id: TerminalId(100),
            name: "shell".to_string(),
            status: TerminalStatus::Stopped,
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
            status: TerminalStatus::Stopped,
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
            id: TerminalId(99),
            name: "shell".to_string(),
            status: TerminalStatus::Stopped,
            launch: TerminalLaunch::Shell,
        };

        assert_eq!(
            terminal_icon_style(&shell_terminal, &pty_runtime, false, palette),
            Style::default().fg(palette.muted)
        );

        shell_terminal.status = TerminalStatus::Running;
        assert_eq!(
            terminal_icon_style(&shell_terminal, &pty_runtime, false, palette),
            Style::default().fg(palette.muted)
        );

        let command_terminal = TerminalSession {
            id: TerminalId(100),
            name: "test".to_string(),
            status: TerminalStatus::Running,
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
            chat_status_style(ChatStatus::Thinking, false, palette),
            Style::default().fg(palette.pine)
        );
        assert_eq!(
            chat_status_style(ChatStatus::Waiting, false, palette),
            Style::default().fg(palette.gold)
        );
        assert_eq!(
            chat_status_style(ChatStatus::Failed, false, palette),
            Style::default().fg(palette.love)
        );
        // Green only while the finished agent has not been seen; gray once seen.
        assert_eq!(
            chat_status_style(ChatStatus::Done, false, palette),
            Style::default().fg(palette.success)
        );
        assert_eq!(
            chat_status_style(ChatStatus::Done, true, palette),
            Style::default().fg(palette.muted)
        );
        assert_eq!(
            chat_status_style(ChatStatus::Idle, false, palette),
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
                draw(
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
        assert_eq!(icon_cell.symbol(), "●");
        assert_eq!(icon_cell.fg, palette.muted);
    }

    #[test]
    fn selected_done_sidebar_agent_icon_is_gray() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.project.workspaces[0].chats[0].status = ChatStatus::Done;
        app.select_item(NavItem::Chat { workspace, chat });

        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                draw(
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
        assert_eq!(icon_cell.symbol(), "●");
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
                draw(
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
        assert_eq!(icon_cell.symbol(), "●");
        // Waiting (the agent is asking the user to pick an option) is yellow,
        // and stays yellow even while selected — only an answer clears it.
        assert_eq!(icon_cell.fg, palette.gold);
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

    #[test]
    fn keybinding_help_line_is_not_rendered() {
        let app = App::default();
        let pty_runtime = PtyRuntime::new_offline();

        let text = draw_text(&app, &pty_runtime, 180, 30);

        assert!(!text.contains("Ctrl-j/k navigate"));
        assert!(!text.contains("mouse wheel scroll"));
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
                draw(
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
        let item_count = sidebar_items(&app, &pty_runtime, test_palette(), 33).len();

        assert_eq!(sidebar_selected_index(&app, item_count), Some(6));
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
                draw(
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

    #[test]
    fn scrolled_terminal_output_hides_cursor() {
        let mut app = App::default();
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
        pty_runtime
            .resize(terminal_id, crate::pty::PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree");
        assert!(pty_runtime.scroll_up(terminal_id, 1).expect("scroll up"));

        let text = draw_text(&app, &pty_runtime, 50, 6);

        assert!(!text.contains('█'));
    }

    #[test]
    fn terminal_cursor_uses_white_on_blank_cell() {
        let mut app = App::default();
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
        pty_runtime.process_terminal_output(terminal_id, b"x");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, &app, &pty_runtime, &config::Config::default()))
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
    fn terminal_text_selection_is_highlighted_in_main_pane() {
        let mut app = App::default();
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
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 0, col: 1 });

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
        pty_runtime.process_terminal_output(terminal_id, b"xy");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        let palette = test_palette();
        let selected_cell = terminal
            .backend()
            .buffer()
            .cell((output_area.x, output_area.y))
            .expect("selected cell is in bounds");
        assert_eq!(selected_cell.symbol(), "x");
        assert_eq!(selected_cell.fg, palette.nc);
        assert_eq!(selected_cell.bg, palette.foam);
    }

    #[test]
    fn wide_char_in_selection_is_highlighted() {
        let mut app = App::default();
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
        // Select 'a' (col 0) through the wide '你' (col 1, spanning cols 1-2).
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 0, col: 1 });

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
        pty_runtime.process_terminal_output(terminal_id, "a你b".as_bytes());

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        let palette = test_palette();
        let buffer = terminal.backend().buffer();
        let narrow = buffer
            .cell((output_area.x, output_area.y))
            .expect("'a' cell is in bounds");
        assert_eq!(narrow.symbol(), "a");
        assert_eq!(narrow.bg, palette.foam);
        // The wide glyph (one grid cell occupying two screen columns) is also
        // highlighted; selecting its cell paints the whole glyph.
        let wide = buffer
            .cell((output_area.x + 1, output_area.y))
            .expect("wide cell is in bounds");
        assert_eq!(wide.bg, palette.foam);
    }

    #[test]
    fn multiline_selection_does_not_highlight_trailing_blanks_of_short_rows() {
        let mut app = App::default();
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
        // Select three rows top-to-bottom; the middle row is shorter than the
        // ones bracketing it.
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 2, col: 4 });

        let frame_area = Rect::new(0, 0, 50, 12);
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
        pty_runtime.process_terminal_output(terminal_id, b"xxxxx\r\nab\r\nyyyyy");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        let palette = test_palette();
        let buffer = terminal.backend().buffer();
        let bg_at = |col: u16, row: u16| {
            buffer
                .cell((output_area.x + col, output_area.y + row))
                .expect("cell is in bounds")
                .bg
        };

        // Middle row "ab": both glyphs are highlighted...
        assert_eq!(bg_at(0, 1), palette.foam);
        assert_eq!(bg_at(1, 1), palette.foam);
        // ...but the blank cells past its content are not, even though the
        // rows above and below extend across that column.
        assert_ne!(bg_at(3, 1), palette.foam);
        assert_eq!(bg_at(3, 0), palette.foam);
        assert_eq!(bg_at(3, 2), palette.foam);
    }

    #[test]
    fn offscreen_terminal_text_selection_is_not_pinned_to_pane_edge() {
        let mut app = App::default();
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
        app.begin_text_selection(terminal_id, SelectionCell { row: -1, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: -1, col: 1 });

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
        pty_runtime.process_terminal_output(terminal_id, b"xy");

        let backend = TestBackend::new(frame_area.width, frame_area.height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, &app, &pty_runtime, &config::Config::default()))
            .expect("draw app");

        let palette = test_palette();
        let cell = terminal
            .backend()
            .buffer()
            .cell((output_area.x, output_area.y))
            .expect("cell is in bounds");
        assert_eq!(cell.symbol(), "x");
        assert_ne!(cell.bg, palette.foam);
    }

    #[test]
    fn terminal_output_does_not_wrap_styled_blank_rows() {
        let mut app = App::default();
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
        draw_text_with_config(app, pty_runtime, &config::Config::default(), width, height)
    }

    fn draw_text_with_config(
        app: &App,
        pty_runtime: &PtyRuntime,
        config: &config::Config,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw(frame, app, pty_runtime, config))
            .expect("draw app");
        format!("{:?}", terminal.backend().buffer())
    }

    fn test_palette() -> Palette {
        Palette::from_colorscheme(&config::Config::default().colorscheme)
    }

    #[test]
    fn readable_fg_keeps_legible_preferred_but_swaps_when_washed_out() {
        let dark = Color::Rgb(31, 29, 48); // moon `nc`
        let light = Color::Rgb(156, 207, 216); // moon `foam`

        // Dark-on-light (the default selection/cursor) is legible, so the
        // preferred foreground is kept verbatim — the default theme is unchanged.
        assert_eq!(readable_fg(dark, light), dark);
        // A light foreground on a light background would wash out: flip to black.
        assert_eq!(readable_fg(light, light), Color::Rgb(0, 0, 0));
        // A dark foreground on a dark background flips to white.
        assert_eq!(readable_fg(dark, dark), Color::Rgb(255, 255, 255));
    }

    #[test]
    fn custom_cursor_and_success_colors_are_themable() {
        let mut colorscheme = config::Config::default().colorscheme;
        colorscheme.cursor = "#010203".to_string();
        colorscheme.success = "#0a0b0c".to_string();

        let palette = Palette::from_colorscheme(&colorscheme);

        assert_eq!(palette.cursor, Color::Rgb(1, 2, 3));
        assert_eq!(palette.success, Color::Rgb(10, 11, 12));
    }

    #[test]
    fn extreme_terminal_sizes_render_without_panicking() {
        let app = App::default();
        let pty_runtime = PtyRuntime::new_offline();

        // Tiny / lopsided frames must not underflow the layout or cursor math.
        // The sidebar is wider than these frames, so the main pane is starved;
        // rendering should still succeed rather than panic.
        for (width, height) in [(1, 1), (2, 3), (20, 5), (1, 40), (40, 1)] {
            let _ = draw_text(&app, &pty_runtime, width, height);
        }
    }

    #[test]
    fn selected_terminal_output_area_tracks_visible_main_pane_size() {
        let mut app = App::default();
        let selected = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Terminal { .. }))
            .expect("seed state has a terminal");
        app.select_nav_index(selected);

        let (_, area) = selected_terminal_output_area(&app, Rect::new(0, 0, 120, 40))
            .expect("terminal selection has output area");

        assert_eq!(area.x, 34);
        assert_eq!(area.y, 0);
        assert_eq!(area.width, 86);
        assert_eq!(area.height, 40);
    }

    #[test]
    fn terminal_output_area_for_tracks_visible_main_pane_without_terminal_selection() {
        let app = App::default();

        let area = terminal_output_area_for(&app, Rect::new(0, 0, 120, 40));

        assert_eq!(area.x, 34);
        assert_eq!(area.y, 0);
        assert_eq!(area.width, 86);
        assert_eq!(area.height, 40);
    }

    #[test]
    fn selected_terminal_output_area_is_absent_for_non_terminal_selection() {
        let app = App::seeded();

        assert_eq!(
            selected_terminal_output_area(&app, Rect::new(0, 0, 120, 40)),
            None
        );
    }

    #[test]
    fn selected_chat_agent_output_area_tracks_visible_main_pane_size() {
        let mut app = App::seeded();
        let selected = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Chat { .. }))
            .expect("seed state has a chat");
        app.select_nav_index(selected);

        let (_, area) = selected_chat_agent_output_area(&app, Rect::new(0, 0, 120, 40))
            .expect("chat selection has pi output area");

        assert_eq!(area.x, 34);
        assert_eq!(area.y, 0);
        assert_eq!(area.width, 86);
        assert_eq!(area.height, 40);
    }
}
