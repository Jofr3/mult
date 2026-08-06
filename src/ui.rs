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
        Prompt, SearchScope, SidebarRow, TextSelection,
    },
    config::{self, ColorSchemeConfig},
    model::{
        ChatId, ChatSession, ChatStatus, PtyKey, TerminalId, TerminalLaunch, TerminalSession,
        Workspace, WorkspaceId,
    },
    pty::PtyRuntime,
};

const SIDEBAR_SELECTION_SYMBOL: &str = " ";
const WORKSPACE_ICON: &str = "▣ ";
const GIT_BRANCH_ICON: &str = "";

/// The built-in Rosé Pine Moon colors, derived at compile time from the very
/// hex strings [`config::DEFAULT_COLOR_SCHEME`] hands the user. There is no
/// second copy of these values to drift: a change there is a change here, and a
/// malformed entry fails the build rather than silently falling back.
mod moon {
    use ratatui::style::Color;

    use super::default_color;
    use crate::config::DEFAULT_COLOR_SCHEME as SCHEME;

    pub const NC: Color = default_color(SCHEME.nc);
    pub const BASE: Color = default_color(SCHEME.base);
    pub const MUTED: Color = default_color(SCHEME.muted);
    pub const TEXT: Color = default_color(SCHEME.text);
    pub const LOVE: Color = default_color(SCHEME.love);
    pub const GOLD: Color = default_color(SCHEME.gold);
    pub const PINE: Color = default_color(SCHEME.pine);
    pub const FOAM: Color = default_color(SCHEME.foam);
    pub const IRIS: Color = default_color(SCHEME.iris);
    pub const HIGHLIGHT_MED: Color = default_color(SCHEME.highlight_med);
    pub const CURSOR: Color = default_color(SCHEME.cursor);
    pub const SUCCESS: Color = default_color(SCHEME.success);
}

/// Const-evaluable `#rrggbb` parse, used only for the built-in defaults. User
/// input goes through [`parse_color`], which reports failure instead.
const fn default_color(hex: &str) -> Color {
    let bytes = hex.as_bytes();
    let offset = match bytes.len() {
        6 => 0,
        7 if bytes[0] == b'#' => 1,
        _ => panic!("a default colorscheme entry must be 6 hex digits, optionally `#`-prefixed"),
    };

    Color::Rgb(
        hex_byte(bytes[offset], bytes[offset + 1]),
        hex_byte(bytes[offset + 2], bytes[offset + 3]),
        hex_byte(bytes[offset + 4], bytes[offset + 5]),
    )
}

const fn hex_byte(high: u8, low: u8) -> u8 {
    hex_digit(high) * 16 + hex_digit(low)
}

const fn hex_digit(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("a default colorscheme entry contains a non-hex digit"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
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
    /// Set when `NO_COLOR` is in the environment (E10). Every colour is
    /// `Color::Reset`, so `mult` emits no SGR colour at all, and the overlays
    /// that were carrying meaning in a background colour — the sidebar
    /// selection, the palette's highlighted row, a text selection, the terminal
    /// cursor — switch to reverse video instead of a hardcoded RGB fallback.
    monochrome: bool,
}

/// A colorscheme key whose configured value did not parse. The palette keeps
/// the built-in default for that key and hands the failure back rather than
/// swallowing it.
///
/// This is the seam a later slice reports startup configuration warnings
/// through; nothing surfaces these yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorParseIssue {
    /// The key as written in `config.json` (note `_nc`, not `nc`).
    pub key: &'static str,
    pub value: String,
}

impl Palette {
    pub(crate) fn from_colorscheme(colorscheme: &ColorSchemeConfig) -> Self {
        Self::from_colorscheme_reporting(colorscheme).0
    }

    pub(crate) fn from_colorscheme_reporting(
        colorscheme: &ColorSchemeConfig,
    ) -> (Self, Vec<ColorParseIssue>) {
        let mut issues = Vec::new();
        let mut parse = |key: &'static str, value: &str, fallback: Color| match parse_color(value) {
            Some(color) => color,
            None => {
                issues.push(ColorParseIssue {
                    key,
                    value: value.to_string(),
                });
                fallback
            }
        };

        let palette = Self {
            nc: parse("_nc", &colorscheme.nc, moon::NC),
            base: parse("base", &colorscheme.base, moon::BASE),
            muted: parse("muted", &colorscheme.muted, moon::MUTED),
            text: parse("text", &colorscheme.text, moon::TEXT),
            love: parse("love", &colorscheme.love, moon::LOVE),
            gold: parse("gold", &colorscheme.gold, moon::GOLD),
            pine: parse("pine", &colorscheme.pine, moon::PINE),
            foam: parse("foam", &colorscheme.foam, moon::FOAM),
            iris: parse("iris", &colorscheme.iris, moon::IRIS),
            highlight_med: parse(
                "highlight_med",
                &colorscheme.highlight_med,
                moon::HIGHLIGHT_MED,
            ),
            cursor: parse("cursor", &colorscheme.cursor, moon::CURSOR),
            success: parse("success", &colorscheme.success, moon::SUCCESS),
            monochrome: false,
        };

        (palette, issues)
    }

    /// The palette used when `NO_COLOR` is set: nothing but the terminal's own
    /// default foreground and background.
    pub(crate) fn monochrome() -> Self {
        Self {
            nc: Color::Reset,
            base: Color::Reset,
            muted: Color::Reset,
            text: Color::Reset,
            love: Color::Reset,
            gold: Color::Reset,
            pine: Color::Reset,
            foam: Color::Reset,
            iris: Color::Reset,
            highlight_med: Color::Reset,
            cursor: Color::Reset,
            success: Color::Reset,
            monochrome: true,
        }
    }

    /// A style that has to stand out from the pane around it: a selected row, a
    /// highlighted match, a cursor overlay. With colour it is `preferred` on
    /// `background`; without it, reverse video, which every terminal has.
    fn emphasis(self, preferred: Color, background: Color) -> Style {
        if self.monochrome {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .fg(readable_fg(preferred, background))
                .bg(background)
        }
    }

    /// The sidebar's selected row. Background (or reverse video) only, and
    /// deliberately no foreground: each row's own status glyph has already
    /// chosen one, and overriding it would put the selected pane's state back
    /// on colour alone (E8).
    fn selection_highlight(self) -> Style {
        if self.monochrome {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .bg(self.highlight_med)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// Emphasis for a whole selected list row in a prompt, where the row is a
    /// single uniform piece of text.
    fn selected_row(self) -> Style {
        if self.monochrome {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .fg(self.text)
                .bg(self.highlight_med)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// Foreground for a semantic accent (an error, a hint, a status glyph).
    /// Without colour the glyph itself carries the meaning (E8), so the only
    /// thing left to say is "this is louder than body text".
    fn accent(self, color: Color, emphatic: bool) -> Style {
        if self.monochrome {
            if emphatic {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            }
        } else {
            Style::default().fg(color)
        }
    }
}

/// Whether `NO_COLOR` is set to a non-empty value.
///
/// Read once: the environment cannot change under a running process in any way
/// this should react to, and `draw` runs on every frame. Tests drive
/// [`draw_with_palette`] with [`Palette::monochrome`] directly rather than
/// mutating a process global.
fn no_color_is_set() -> bool {
    static NO_COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_COLOR.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.as_encoded_bytes().is_empty())
    })
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
    let palette = if no_color_is_set() {
        Palette::monochrome()
    } else {
        config.palette()
    };
    draw_with_palette(frame, app, pty_runtime, config, palette);
}

/// [`draw`] with the palette decided by the caller. `NO_COLOR` is a process
/// global, so this is the seam the render tests use instead of mutating it.
pub(crate) fn draw_with_palette(
    frame: &mut Frame,
    app: &App,
    pty_runtime: &PtyRuntime,
    config: &config::Config,
    palette: Palette,
) {
    let frame_area = frame.area();
    let layout = layout_areas(app, frame_area);

    draw_sidebar(frame, app, pty_runtime, layout.sidebar, palette);
    draw_main(frame, app, pty_runtime, layout.main, palette);
    draw_prompt_area(frame, app, layout.prompt, palette, config);
    // Last, and over everything: the overlay is modal, so it must not be
    // painted under a pane, and it occupies no layout space when it is down.
    if app.is_help_visible() {
        draw_help_overlay(frame, frame_area, palette);
    }
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

/// Terminal and chat panes are drawn with neither a border nor a header, so a
/// pane's output area is its whole area. Until F17 this went through a
/// `pane_inner` / `output_area_after_header` pair whose two header constants
/// were both `0`.
pub fn terminal_output_area_for(app: &App, frame_area: Rect) -> Rect {
    layout_areas(app, frame_area).main
}

pub fn chat_agent_output_area_for(app: &App, frame_area: Rect) -> Rect {
    layout_areas(app, frame_area).main
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
    let prompt_height = match &app.prompt {
        Some(Prompt::CommandPalette(_)) => 7,
        Some(Prompt::OpenWorkspace(prompt))
            if prompt.mode == OpenWorkspaceMode::ConfiguredProjects =>
        {
            7
        }
        Some(_) => 3,
        None => 0,
    };
    // The status surface only exists while it has something to say, so a quiet
    // session gives every row back to the panes (E2).
    let notice_height = u16::try_from(app.notices().len()).unwrap_or(u16::MAX);
    prompt_height + notice_height
}

fn draw_sidebar(
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
                            Span::raw(chat_sidebar_label(chat)),
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

fn text_width(value: &str) -> usize {
    Span::raw(value).width()
}

/// Display width of a single character. `Span::raw` borrows, so encoding into a
/// stack buffer keeps this allocation-free — this runs per character of every
/// sidebar row, twice per row, every frame.
fn char_width(ch: char) -> usize {
    let mut buffer = [0u8; 4];
    text_width(ch.encode_utf8(&mut buffer))
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
        let ch_width = char_width(ch);
        if width + ch_width + ellipsis_width > max_width {
            break;
        }
        output.push(ch);
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

fn render_terminal_parser(
    frame: &mut Frame,
    area: Rect,
    parser: &vt100::Parser,
    focused: bool,
    palette: Palette,
    selection: Option<&TextSelection>,
) {
    if !area_fits_a_screen(area) {
        render_pane_too_small(frame, area, focused, palette);
        return;
    }
    // Without colour the block would be invisible against the default
    // foreground, so the cursor cell is reverse video instead (E10).
    let cursor_style = if palette.monochrome {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(palette.cursor).bg(palette.base)
    };
    let cursor_overlay_style = palette.emphasis(palette.nc, palette.cursor);
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

/// Whether a pane area can hold a screen at the size the emulator is actually
/// driven at.
///
/// The parser is never built below `MIN_SCREEN_ROWS`×`MIN_SCREEN_COLS`
/// (see [`crate::pty::PtyDimensions::clamped`] — anything smaller panics
/// `fnug-vt100`), so a smaller area cannot show its screen: it would render a
/// crop of a differently sized terminal, with the cursor and every mouse
/// coordinate off. Say so instead.
fn area_fits_a_screen(area: Rect) -> bool {
    crate::pty::PtyDimensions {
        rows: area.height,
        cols: area.width,
    }
    .fits_a_screen()
}

/// Fill a pane that is too small to draw with a visible marker. Anything wider
/// than a couple of columns gets words; below that the pane is filled with `!`
/// so the degradation still reads as deliberate rather than as a blank pane.
fn render_pane_too_small(frame: &mut Frame, area: Rect, focused: bool, palette: Palette) {
    let style = pane_style(focused, palette).fg(palette.gold);
    let label = if area.width >= 9 {
        "too small".to_string()
    } else {
        "!".repeat(usize::from(area.width))
    };
    let lines = (0..area.height)
        .map(|row| {
            if row == area.height / 2 {
                Line::from(label.clone())
            } else {
                Line::from("")
            }
        })
        .collect::<Vec<_>>();
    let paragraph = Paragraph::new(lines)
        .block(Block::default().style(style))
        .style(style);
    frame.render_widget(paragraph, area);
}

/// Bytes of cell contents stored inline. `vt100` caps a cell at six `char`s
/// (`CODEPOINTS_IN_CELL`) — a base character plus combining marks — so 6 × 4
/// bytes covers every cell the parser can produce, and the heap arm below is a
/// safety net rather than a live path.
const INLINE_SYMBOL_BYTES: usize = 24;

/// A cell's text, kept out of the heap. `TerminalScreen` rebuilds every cell on
/// every frame, so a `String` here was one allocation per cell per frame.
#[derive(Debug, Clone)]
enum CellSymbol {
    Inline {
        bytes: [u8; INLINE_SYMBOL_BYTES],
        len: u8,
    },
    Spilled(Box<str>),
}

impl CellSymbol {
    const EMPTY: Self = Self::Inline {
        bytes: [0; INLINE_SYMBOL_BYTES],
        len: 0,
    };

    fn new(contents: &str) -> Self {
        let Ok(len) = u8::try_from(contents.len()) else {
            return Self::Spilled(Box::from(contents));
        };
        if usize::from(len) > INLINE_SYMBOL_BYTES {
            return Self::Spilled(Box::from(contents));
        }

        let mut bytes = [0; INLINE_SYMBOL_BYTES];
        bytes[..contents.len()].copy_from_slice(contents.as_bytes());
        Self::Inline { bytes, len }
    }

    fn as_str(&self) -> &str {
        match self {
            // Always built from a `&str`, so the slice is valid UTF-8 by
            // construction; the fallback keeps this free of `unsafe`.
            Self::Inline { bytes, len } => {
                std::str::from_utf8(&bytes[..usize::from(*len)]).unwrap_or_default()
            }
            Self::Spilled(contents) => contents,
        }
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
    symbol: CellSymbol,
    has_contents: bool,
    style: Style,
}

impl TerminalScreen {
    fn from_vt100(screen: &vt100::Screen) -> Self {
        let (rows, cols) = screen.size();
        let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
        for row in 0..rows {
            for col in 0..cols {
                cells.push(
                    screen
                        .cell(row, col)
                        .map(TerminalCell::from_vt100)
                        .unwrap_or_default(),
                );
            }
        }
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
            symbol: CellSymbol::EMPTY,
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

        // `vt100::Cell::contents` builds an owned `String`, and a blank cell's
        // symbol is never rendered (see `apply`), so blanks skip the call
        // entirely and the rest copy the result inline instead of cloning it
        // onto the heap a second time.
        let has_contents = cell.has_contents();
        let symbol = if has_contents {
            CellSymbol::new(&cell.contents())
        } else {
            CellSymbol::EMPTY
        };

        Self {
            symbol,
            has_contents,
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
            cell.set_symbol(self.symbol.as_str());
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
    let style = palette.emphasis(palette.nc, palette.foam);

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
    let notices = app.notices();
    let notice_height = u16::try_from(notices.len()).unwrap_or(u16::MAX);
    let [notice_area, prompt_area] =
        Layout::vertical([Constraint::Length(notice_height), Constraint::Min(0)]).areas(area);
    if !notices.is_empty() {
        let lines = notices
            .iter()
            .map(|notice| {
                Line::from(Span::styled(
                    format!("{} {}", notice_marker(notice.level()), notice.text()),
                    notice_style(notice.level(), palette),
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(palette.text).bg(palette.base)),
            notice_area,
        );
    }

    let Some(prompt) = &app.prompt else {
        return;
    };
    let area = prompt_area;

    match prompt {
        Prompt::OpenWorkspace(prompt) if prompt.mode == OpenWorkspaceMode::ConfiguredProjects => {
            draw_open_workspace_prompt(
                frame,
                area,
                palette,
                &prompt.input,
                prompt.selected.index(),
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
            prompt.selected.index(),
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
        Prompt::ConfirmDelete(prompt) => draw_delete_confirmation_prompt(
            frame,
            area,
            palette,
            &prompt.description,
            prompt.error.as_deref(),
        ),
    }
}

fn draw_delete_confirmation_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    description: &str,
    error: Option<&str>,
) {
    let mut lines = vec![Line::from(vec![
        Span::styled("Delete ", Style::default().fg(palette.love)),
        Span::raw(description.to_string()),
        Span::raw("?"),
    ])];
    if let Some(error) = error {
        lines.push(Line::from(Span::styled(
            error.to_string(),
            Style::default().fg(palette.love),
        )));
    }
    lines.push(Line::from(Span::styled(
        "enter confirms • esc/ctrl-c cancels",
        Style::default().fg(palette.muted),
    )));
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(palette.text).bg(palette.base)),
        area,
    );
}

/// The keybinding overlay (E4).
///
/// Generated from [`crate::app::BINDINGS`], the same table the command palette
/// filters, so a binding cannot be added to one and forgotten in the other.
/// It is drawn over the frame rather than in the layout, so it costs nothing
/// while it is down, and it degrades by truncation on a small terminal instead
/// of overflowing: rows past the bottom are dropped and a footer says so.
fn draw_help_overlay(frame: &mut Frame, frame_area: Rect, palette: Palette) {
    if frame_area.is_empty() {
        return;
    }

    // The overlay is a bordered panel as wide as its widest row and no wider
    // than the frame. A fixed 64-column cap clipped the longest labels mid-word
    // — "Move through results (palette, projec" — with nothing on screen to say
    // they had been cut, and without a border it ran straight into whatever
    // pane it covered ("▣ websKeybindings"). `CHROME` is the two columns and two
    // rows the border costs.
    const KEY_GAP: usize = 2;
    const CHROME: u16 = 2;
    let key_width = crate::app::BINDINGS
        .iter()
        .filter_map(|binding| binding.keys)
        .map(text_width)
        .max()
        .unwrap_or(0);
    let label_width = crate::app::BINDINGS
        .iter()
        .filter(|binding| binding.keys.is_some())
        .map(|binding| text_width(binding.label))
        .max()
        .unwrap_or(0);
    let footer = "esc / ? / F1 closes • ctrl-p opens the command palette";
    let natural_width = (key_width + KEY_GAP + label_width).max(text_width(footer));
    let width = u16::try_from(natural_width)
        .unwrap_or(u16::MAX)
        .saturating_add(CHROME)
        .clamp(1, frame_area.width);
    // What is left for a label once the border, the key column and its gap are
    // paid for. Zero on a terminal narrower than the key column itself, where
    // the rows are already degenerate.
    let label_budget =
        usize::from(width.saturating_sub(CHROME)).saturating_sub(key_width + KEY_GAP);

    let mut lines = vec![Line::from(Span::styled(
        "Keybindings",
        Style::default()
            .fg(palette.foam)
            .add_modifier(Modifier::BOLD),
    ))];
    for scope in [
        crate::app::BindingScope::Global,
        crate::app::BindingScope::Prompt,
        crate::app::BindingScope::Mouse,
    ] {
        let mut heading_written = false;
        for binding in crate::app::BINDINGS
            .iter()
            .filter(|binding| binding.scope == scope)
        {
            let Some(keys) = binding.keys else {
                // Palette-only commands have no key to list; the palette itself
                // is where they are discovered, and `Ctrl+p` is listed above.
                continue;
            };
            if !heading_written {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    scope.title(),
                    palette.accent(palette.iris, true),
                )));
                heading_written = true;
            }
            let padding = " ".repeat(key_width.saturating_sub(text_width(keys)));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{keys}{padding}"),
                    palette.accent(palette.gold, false),
                ),
                Span::raw(" ".repeat(KEY_GAP)),
                // Truncated with an ellipsis rather than clipped by the
                // terminal, so a cut label says that it was cut.
                Span::raw(truncate_text(binding.label, label_budget)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        footer,
        palette.accent(palette.muted, false),
    )));

    // Centre the overlay, but never let it exceed the frame: on a terminal too
    // small for the whole list the visible part is still correct.
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(CHROME)
        .clamp(1, frame_area.height);
    let content_height = usize::from(height.saturating_sub(CHROME));
    if content_height < lines.len() {
        // Spend the last visible row saying the list is cut off rather than
        // ending mid-table with no explanation.
        lines.truncate(content_height.saturating_sub(1));
        lines.push(Line::from(Span::styled(
            "… resize for the rest",
            palette.accent(palette.muted, false),
        )));
    }
    let area = Rect {
        x: frame_area.x + (frame_area.width - width) / 2,
        y: frame_area.y + (frame_area.height - height) / 2,
        width,
        height,
    };

    let style = Style::default().fg(palette.text).bg(palette.base);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_style(Style::default().fg(palette.muted).bg(palette.base))
                    .style(style),
            )
            .style(style),
        area,
    );
}

fn search_prompt_label(scope: SearchScope) -> &'static str {
    match scope {
        SearchScope::Terminal(_) => "Search terminal: ",
        SearchScope::Chat(_) => "Search chat: ",
    }
}

/// The prompt's label, its text and its cursor, as spans.
///
/// The text is emitted verbatim in up to three spans — before the cursor, the
/// single character under it, and after it — so concatenating them reproduces
/// the stored string exactly. That is what keeps the cursor at the right
/// *display* column for wide and combining characters: the character under the
/// cursor is styled, never substituted, so it still occupies its own width.
/// The only span that is not part of the text is the block drawn when the
/// cursor sits past the last character, where there is no cell to style (E7).
fn prompt_input_spans(
    label: &'static str,
    input: &crate::app::PromptInput,
    palette: Palette,
) -> Vec<Span<'static>> {
    let (before, at, after) = input.split_at_cursor();
    let mut spans = vec![
        Span::styled(label, palette.accent(palette.muted, false)),
        Span::raw(before.to_string()),
    ];
    if at.is_empty() {
        spans.push(Span::styled(
            "▌",
            palette.accent(palette.cursor, palette.monochrome),
        ));
    } else {
        spans.push(Span::styled(
            at.to_string(),
            palette.emphasis(palette.nc, palette.cursor),
        ));
        spans.push(Span::raw(after.to_string()));
    }
    spans
}

fn draw_open_workspace_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    input: &crate::app::PromptInput,
    selected: usize,
    error: Option<&str>,
    entries: Vec<OpenWorkspaceMatch>,
) {
    let mut lines = vec![Line::from(prompt_input_spans("Project: ", input, palette))];

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
        palette.selected_row()
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
    input: &crate::app::PromptInput,
    selected: usize,
    entries: Vec<CommandPaletteEntry>,
) {
    let mut lines = vec![
        Line::from(prompt_input_spans("Command: ", input, palette)),
        Line::from(Span::styled(
            "type to filter • ↑/↓ select • enter runs • esc cancels".to_string(),
            palette.accent(palette.muted, false),
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
        palette.selected_row()
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
    input: &crate::app::PromptInput,
    error: Option<&str>,
    help: &'static str,
) {
    let message = error.unwrap_or(help);
    let message_style = if error.is_some() {
        palette.accent(palette.love, true)
    } else {
        palette.accent(palette.muted, false)
    };
    let prompt = Paragraph::new(vec![
        Line::from(prompt_input_spans(label, input, palette)),
        Line::from(Span::styled(message.to_string(), message_style)),
    ])
    .style(Style::default().fg(palette.text).bg(palette.base));
    frame.render_widget(prompt, area);
}

/// Sidebar label for a chat, tagged with the agent backing it so running
/// agents are distinguishable at a glance, e.g. `agent: pi` or `agent: cc`.
fn chat_sidebar_label(chat: &ChatSession) -> String {
    format!("{}: {}", chat.name, chat.agent.label())
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

fn notice_marker(level: crate::app::NoticeLevel) -> &'static str {
    match level {
        crate::app::NoticeLevel::Info => "i",
        crate::app::NoticeLevel::Warning => "!",
        crate::app::NoticeLevel::Error => "✗",
    }
}

fn notice_style(level: crate::app::NoticeLevel, palette: Palette) -> Style {
    match level {
        crate::app::NoticeLevel::Info => palette.accent(palette.foam, false),
        crate::app::NoticeLevel::Warning => palette.accent(palette.gold, true),
        crate::app::NoticeLevel::Error => palette.accent(palette.love, true),
    }
}

fn terminal_has_active_command(terminal: &TerminalSession, pty_runtime: &PtyRuntime) -> bool {
    matches!(terminal.launch, TerminalLaunch::Command(_))
        && pty_runtime.is_running(PtyKey::Terminal(terminal.id))
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    use super::*;
    use crate::app::SelectionCell;
    use crate::model::AgentKind;

    #[test]
    fn chat_sidebar_label_tags_the_agent_kind() {
        let mut chat = ChatSession {
            id: ChatId(1),
            name: "agent".to_string(),
            status: ChatStatus::Idle,
            agent: AgentKind::Pi,
            messages: Vec::new(),
        };
        assert_eq!(chat_sidebar_label(&chat), "agent: pi");

        chat.agent = AgentKind::ClaudeCode;
        assert_eq!(chat_sidebar_label(&chat), "agent: cc");
    }

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
        assert_eq!(icon_cell.symbol(), "○");
        assert_eq!(icon_cell.fg, palette.muted);
    }

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
        assert_eq!(icon_cell.symbol(), "?");
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
    fn save_failure_is_rendered_persistently_without_a_prompt() {
        let mut app = App::default();
        app.record_save_failure("disk full");

        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);

        assert!(text.contains("State save failed: disk full"));
        assert!(text.contains("edit or quit to retry"));
    }

    #[test]
    fn delete_confirmation_names_the_target_and_controls() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        assert!(app.begin_delete_selected());

        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 30);

        assert!(text.contains("Delete terminal"));
        assert!(text.contains("enter confirms"));
        assert!(text.contains("esc/ctrl-c cancels"));
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
        format!(
            "{:?}",
            render_buffer(app, pty_runtime, config, width, height)
        )
    }

    fn render_buffer(
        app: &App,
        pty_runtime: &PtyRuntime,
        config: &config::Config,
        width: u16,
        height: u16,
    ) -> Buffer {
        render_buffer_with_palette(app, pty_runtime, config, config.palette(), width, height)
    }

    /// `NO_COLOR` is a process global that no test may mutate (G7), so the
    /// monochrome frames drive the palette seam instead.
    fn render_buffer_with_palette(
        app: &App,
        pty_runtime: &PtyRuntime,
        config: &config::Config,
        palette: Palette,
        width: u16,
        height: u16,
    ) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| draw_with_palette(frame, app, pty_runtime, config, palette))
            .expect("draw app");
        terminal.backend().buffer().clone()
    }

    /// Render a whole `TestBackend` buffer as a snapshot: the glyph grid, a
    /// parallel grid of per-cell style keys, and a legend resolving them. The
    /// style grid is the point — `.contains(…)` assertions cannot see a sidebar
    /// that changed width, a header that moved, or a selection that stopped
    /// painting, and all three are pane-background changes before they are text
    /// changes.
    ///
    /// Rows are bracketed with `|` so trailing blanks are visible. A wide glyph
    /// leaves its successor cell's symbol empty (ratatui's own convention), so
    /// the symbol row stays as wide as the terminal while the style row always
    /// has exactly one key per cell.
    fn buffer_snapshot(buffer: &Buffer) -> String {
        let area = buffer.area();
        let mut legend: Vec<Style> = Vec::new();
        let mut symbol_rows = Vec::new();
        let mut style_rows = Vec::new();

        for y in area.top()..area.bottom() {
            let mut symbols = String::new();
            let mut styles = String::new();
            for x in area.left()..area.right() {
                let cell = buffer.cell((x, y)).expect("cell is in bounds");
                symbols.push_str(cell.symbol());
                let style = cell.style();
                let key = legend.iter().position(|known| *known == style);
                styles.push(legend_key(key.unwrap_or_else(|| {
                    legend.push(style);
                    legend.len() - 1
                })));
            }
            symbol_rows.push(symbols);
            style_rows.push(styles);
        }

        let mut snapshot = format!("size: {}x{}\n\nsymbols:\n", area.width, area.height);
        for row in &symbol_rows {
            snapshot.push_str(&format!("|{row}|\n"));
        }
        snapshot.push_str("\nstyles:\n");
        for row in &style_rows {
            snapshot.push_str(&format!("|{row}|\n"));
        }
        snapshot.push_str("\nlegend:\n");
        for (index, style) in legend.iter().enumerate() {
            snapshot.push_str(&format!("  {} = {style:?}\n", legend_key(index)));
        }
        snapshot
    }

    fn legend_key(index: usize) -> char {
        const KEYS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        KEYS.get(index).map_or('?', |key| char::from(*key))
    }

    /// The seed app with a terminal selected and `output` fed to a parser sized
    /// to the visible output area, so terminal snapshots do not depend on how
    /// the runtime happened to size the pane.
    fn terminal_app_with_output(frame_area: Rect, output: &[u8]) -> (App, PtyRuntime, PtyKey) {
        let mut app = App::default();
        let selected = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Terminal { .. }))
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        let (terminal_id, output_area) =
            selected_terminal_output_area(&app, frame_area).expect("terminal has an output area");
        let terminal_id = PtyKey::Terminal(terminal_id);

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
        pty_runtime.process_terminal_output(terminal_id, output);

        (app, pty_runtime, terminal_id)
    }

    // Snapshot scenarios deliberately avoid the chat pane's "not started" hint,
    // which prints `config::config_path()` and is therefore `$HOME`-dependent,
    // and never populate git branches, which are supplied by the caller rather
    // than probed here. Nothing else in `draw` reads the environment or a clock.

    #[test]
    fn snapshot_default_frame() {
        let app = App::default();

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            100,
            30,
        )));
    }

    #[test]
    fn snapshot_narrow_frame_80x24() {
        let app = App::default();

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            80,
            24,
        )));
    }

    #[test]
    fn snapshot_command_palette_prompt() {
        let mut app = App::default();
        app.begin_command_palette();
        app.push_prompt_char('t');

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            100,
            30,
        )));
    }

    #[test]
    fn snapshot_terminal_with_scrollback_and_selection() {
        let frame_area = Rect::new(0, 0, 60, 12);
        let (mut app, mut pty_runtime, terminal_id) = terminal_app_with_output(
            frame_area,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\neight\r\nnine\r\nten\r\neleven\r\ntwelve\r\nthirteen",
        );
        assert!(pty_runtime.scroll_up(terminal_id, 2).expect("scroll up"));
        app.begin_text_selection(terminal_id, SelectionCell { row: 1, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 2, col: 3 });

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &pty_runtime,
            &config::Config::default(),
            frame_area.width,
            frame_area.height,
        )));
    }

    #[test]
    fn snapshot_help_overlay() {
        // E4: the overlay is generated from `app::BINDINGS`, so this snapshot
        // is also the check that adding a binding without a label, or a
        // palette command without a key, changes what the user is shown.
        let mut app = App::default();
        app.show_help();

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            100,
            40,
        )));
    }

    #[test]
    fn snapshot_status_surface_with_an_error() {
        // E2: a daemon that will not connect, a save that failed and a config
        // warning all land in one dismissible surface that exists only while
        // it has something to say.
        let mut app = App::default();
        app.push_notice(
            crate::app::NoticeLevel::Error,
            crate::app::NoticeSource::Report,
            "failed to connect to mult-server: No such file or directory (os error 2)",
        );
        app.push_notice(
            crate::app::NoticeLevel::Warning,
            crate::app::NoticeSource::Report,
            "config.json: colorscheme.gold is not a #rrggbb color",
        );
        app.record_save_failure("Read-only file system (os error 30)");

        insta::assert_snapshot!(buffer_snapshot(&render_buffer(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            100,
            30,
        )));
    }

    #[test]
    fn snapshot_no_color_frame() {
        // E10: every colour is `Color::Reset`, and the selected sidebar row is
        // reverse video rather than a background colour, so the frame still
        // reads. The style legend is the assertion here.
        let mut app = App::default();
        app.project.workspaces[0]
            .chats
            .push(crate::model::ChatSession {
                id: ChatId(4242),
                name: "agent".to_string(),
                status: ChatStatus::Waiting,
                agent: AgentKind::Pi,
                messages: Vec::new(),
            });

        insta::assert_snapshot!(buffer_snapshot(&render_buffer_with_palette(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            Palette::monochrome(),
            100,
            30,
        )));
    }

    #[test]
    fn no_color_emits_no_color_at_all_and_keeps_overlays_distinguishable() {
        let mut app = App::default();
        let selected = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Terminal { .. }))
            .expect("seed state has a terminal");
        app.select_nav_index(selected);

        let buffer = render_buffer_with_palette(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            Palette::monochrome(),
            100,
            30,
        );

        // Not one truecolor or indexed escape: with `NO_COLOR` the terminal's
        // own defaults are the only colours used.
        for y in buffer.area().top()..buffer.area().bottom() {
            for x in buffer.area().left()..buffer.area().right() {
                let cell = buffer.cell((x, y)).expect("cell is in bounds");
                assert_eq!(cell.fg, Color::Reset, "cell ({x},{y}) painted a foreground");
                assert_eq!(cell.bg, Color::Reset, "cell ({x},{y}) painted a background");
            }
        }

        // The selected sidebar row is still marked — by an attribute, not by a
        // hardcoded RGB fallback, which is the trap E10 exists to avoid.
        let reversed_rows = (buffer.area().top()..buffer.area().bottom())
            .filter(|y| {
                buffer
                    .cell((0, *y))
                    .is_some_and(|cell| cell.modifier.contains(Modifier::REVERSED))
            })
            .count();
        assert_eq!(
            reversed_rows, 1,
            "exactly the selected sidebar row must be reverse video"
        );
    }

    #[test]
    fn a_prompt_cursor_styles_a_wide_character_without_rewriting_the_text() {
        // E7: the spans that carry text must concatenate back to the stored
        // string exactly, so the cursor lands on the right display column even
        // when the character under it is double-width or carries a combining
        // mark.
        let palette = test_palette();
        let mut input = crate::app::PromptInput::new("a日e\u{0301}b");
        assert!(input.apply(crate::app::PromptEdit::MoveHome));
        assert!(input.apply(crate::app::PromptEdit::MoveRight));

        let spans = prompt_input_spans("Path: ", &input, palette);
        // The label is span 0; the rest is the text, verbatim.
        assert_eq!(spans[0].content, "Path: ");
        let text = spans[1..]
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, input.as_str());
        // The cursor is on the wide character itself, styled rather than
        // replaced, so it keeps its two columns.
        assert_eq!(spans[2].content, "日");
        assert_eq!(text_width(&spans[1].content), 1);
        assert_eq!(text_width(&spans[2].content), 2);

        // Past the last character there is no cell to style, so the block is
        // appended — the one span that is not part of the text.
        assert!(input.apply(crate::app::PromptEdit::MoveEnd));
        let spans = prompt_input_spans("Path: ", &input, palette);
        assert_eq!(spans[1].content, input.as_str());
        assert_eq!(spans[2].content, "▌");
    }

    #[test]
    fn the_help_overlay_survives_a_terminal_too_small_to_hold_it() {
        let mut app = App::default();
        app.show_help();

        // Down to a single cell: rendering must not panic, and anything with
        // room for words must say the list is cut off rather than end
        // mid-table.
        for (width, height) in [(1, 1), (20, 4), (40, 8), (100, 12)] {
            let text = draw_text(&app, &PtyRuntime::new_offline(), width, height);
            if width >= 40 {
                assert!(
                    text.contains("resize for the rest"),
                    "{width}x{height} truncated the overlay silently"
                );
            }
        }

        // With room for everything, no truncation notice.
        let text = draw_text(&app, &PtyRuntime::new_offline(), 100, 40);
        assert!(text.contains("Keybindings"));
        assert!(text.contains("Ctrl+p"));
        assert!(!text.contains("resize for the rest"));
    }

    /// E4: the overlay used to be capped at 64 columns whatever the terminal
    /// was, so its longest labels were clipped mid-word by the renderer — "Move
    /// through results (palette, projec" — with nothing to mark the cut. It is
    /// now as wide as its widest row, and a label it genuinely cannot fit ends
    /// in an ellipsis.
    #[test]
    fn the_help_overlay_fits_its_labels_and_marks_the_ones_it_cannot() {
        let mut app = App::default();
        app.show_help();
        let longest = crate::app::BINDINGS
            .iter()
            .filter(|binding| binding.keys.is_some())
            .map(|binding| binding.label)
            .max_by_key(|label| text_width(label))
            .expect("the table has bindings with keys");

        let roomy = draw_text(&app, &PtyRuntime::new_offline(), 120, 40);
        assert!(
            roomy.contains(longest),
            "the widest label must be shown whole: {longest:?}"
        );
        assert!(
            !roomy.contains('…'),
            "nothing was cut, so nothing is marked"
        );

        // Too narrow for the longest label, but wide enough to say so.
        let narrow = draw_text(&app, &PtyRuntime::new_offline(), 60, 40);
        assert!(!narrow.contains(longest));
        assert!(
            narrow.contains('…'),
            "a clipped label must say it was clipped"
        );
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

    fn vt100_parser(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        parser
    }

    /// Every symbol painted inside `area`, in reading order.
    fn painted_area(backend: &TestBackend, area: Rect) -> String {
        let buffer = backend.buffer();
        (0..area.height)
            .flat_map(|row| (0..area.width).map(move |col| (row, col)))
            .map(|(row, col)| {
                buffer
                    .cell((area.x + col, area.y + row))
                    .expect("cell is in bounds")
                    .symbol()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_pane_below_the_emulator_floor_is_marked_too_small() {
        // The parser is never built below 2×2 (A13), so a one-row or one-column
        // pane cannot show its screen. It must say so rather than paint a crop
        // of a differently sized terminal.
        let parser = vt100_parser(2, 4, b"WXYZ");
        let palette = test_palette();

        for area in [
            Rect::new(0, 0, 1, 4),
            Rect::new(0, 0, 4, 1),
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 20, 1),
        ] {
            let backend = TestBackend::new(24, 8);
            let mut terminal = Terminal::new(backend).expect("create test terminal");
            terminal
                .draw(|frame| render_terminal_parser(frame, area, &parser, true, palette, None))
                .expect("draw pane");

            let painted = painted_area(terminal.backend(), area);
            assert!(
                !painted.contains('W'),
                "{area:?} drew the screen at the wrong size: {painted:?}"
            );
            if area.width >= 9 {
                assert!(
                    painted.contains("too small"),
                    "{area:?} did not say why it is blank: {painted:?}"
                );
            } else {
                assert!(
                    painted.contains('!'),
                    "{area:?} degraded invisibly: {painted:?}"
                );
            }
        }
    }

    #[test]
    fn a_pane_at_the_emulator_floor_still_draws_its_screen() {
        let parser = vt100_parser(2, 2, b"ab");
        let area = Rect::new(0, 0, 2, 2);
        let backend = TestBackend::new(8, 8);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| render_terminal_parser(frame, area, &parser, true, test_palette(), None))
            .expect("draw pane");

        assert!(painted_area(terminal.backend(), area).starts_with("ab"));
    }

    #[test]
    fn vt100_attributes_map_to_ratatui_modifiers() {
        let parser = vt100_parser(
            1,
            8,
            b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[4mU\x1b[0m\x1b[7mR\x1b[0m\x1b[1;3;4;7mA\x1b[0m",
        );
        let screen = TerminalScreen::from_vt100(parser.screen());
        let cell = |col| screen.cell(0, col).expect("cell is in bounds");

        assert_eq!(cell(0).symbol.as_str(), "B");
        assert_eq!(cell(0).style.add_modifier, Modifier::BOLD);
        assert_eq!(cell(1).style.add_modifier, Modifier::ITALIC);
        assert_eq!(cell(2).style.add_modifier, Modifier::UNDERLINED);
        assert_eq!(cell(3).style.add_modifier, Modifier::REVERSED);
        assert_eq!(
            cell(4).style.add_modifier,
            Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::REVERSED
        );
        // An untouched cell carries no attributes at all.
        assert_eq!(cell(5).style.add_modifier, Modifier::empty());
    }

    #[test]
    fn vt100_color_variants_map_to_reset_indexed_and_rgb() {
        assert_eq!(vt100_color_to_ratatui(vt100::Color::Default), Color::Reset);
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Idx(9)),
            Color::Indexed(9)
        );
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Rgb(1, 2, 3)),
            Color::Rgb(1, 2, 3)
        );

        // ...and the same three shapes as the adapter sees them: default,
        // an SGR palette index, and a 24-bit foreground over an indexed
        // background.
        let parser = vt100_parser(
            1,
            8,
            b"d\x1b[31mi\x1b[0m\x1b[38;2;10;20;30m\x1b[48;5;42mr\x1b[0m",
        );
        let screen = TerminalScreen::from_vt100(parser.screen());
        let cell = |col| screen.cell(0, col).expect("cell is in bounds");

        assert_eq!(cell(0).style.fg, Some(Color::Reset));
        assert_eq!(cell(0).style.bg, Some(Color::Reset));
        assert_eq!(cell(1).style.fg, Some(Color::Indexed(1)));
        assert_eq!(cell(2).style.fg, Some(Color::Rgb(10, 20, 30)));
        assert_eq!(cell(2).style.bg, Some(Color::Indexed(42)));
    }

    #[test]
    fn wide_cell_occupies_one_symbol_and_blank_successor() {
        let parser = vt100_parser(1, 8, "你a".as_bytes());
        let screen = TerminalScreen::from_vt100(parser.screen());
        let cell = |col| screen.cell(0, col).expect("cell is in bounds");

        // The wide glyph lives in a single grid cell and carries the whole
        // character; the column it visually covers is its continuation.
        assert!(cell(0).has_contents());
        assert_eq!(cell(0).symbol.as_str(), "你");
        // vt100 marks a wide continuation by setting a flag bit in the same
        // byte that stores the length, so `has_contents` reports `true` for it
        // while its *contents* are empty. Keying the symbol off the contents —
        // not off the flag — is what stops the pair being overprinted with a
        // stray glyph, and it is why the adapter cannot treat `has_contents` as
        // "non-empty".
        assert!(cell(1).has_contents());
        assert_eq!(cell(1).symbol.as_str(), "");
        // The next character resumes at the column after the pair.
        assert!(cell(2).has_contents());
        assert_eq!(cell(2).symbol.as_str(), "a");

        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let target = &mut buffer[(0, 0)];
        target.set_symbol("stale");
        cell(1).apply(target);
        assert_eq!(target.symbol(), "");
    }

    #[test]
    fn cell_symbols_survive_combining_marks_and_the_inline_capacity_boundary() {
        // vt100 packs a base character plus combining marks into one cell, up
        // to six codepoints; all of them must reach the buffer.
        let with_marks = "e\u{0301}\u{0302}\u{0303}\u{0304}\u{0305}";
        let parser = vt100_parser(1, 4, with_marks.as_bytes());
        let screen = TerminalScreen::from_vt100(parser.screen());
        assert_eq!(
            screen
                .cell(0, 0)
                .expect("cell is in bounds")
                .symbol
                .as_str(),
            with_marks
        );

        // Six four-byte codepoints is the largest cell vt100 can build, and it
        // is exactly the inline capacity: it must stay inline and round-trip.
        let at_capacity = "\u{1f600}".repeat(6);
        assert_eq!(at_capacity.len(), INLINE_SYMBOL_BYTES);
        assert!(matches!(
            CellSymbol::new(&at_capacity),
            CellSymbol::Inline { .. }
        ));
        assert_eq!(CellSymbol::new(&at_capacity).as_str(), at_capacity);

        // One byte past spills to the heap rather than truncating.
        let past_capacity = format!("{at_capacity}a");
        assert!(matches!(
            CellSymbol::new(&past_capacity),
            CellSymbol::Spilled(_)
        ));
        assert_eq!(CellSymbol::new(&past_capacity).as_str(), past_capacity);

        assert_eq!(CellSymbol::EMPTY.as_str(), "");
        assert_eq!(TerminalCell::default().symbol.as_str(), "");
    }

    fn test_palette() -> Palette {
        config::Config::default().palette()
    }

    /// `ColorSchemeConfig` carries a private palette cache, so its fields
    /// cannot be filled with functional-update syntax from here.
    fn colorscheme_with(mutate: impl FnOnce(&mut ColorSchemeConfig)) -> ColorSchemeConfig {
        let mut colorscheme = ColorSchemeConfig::default();
        mutate(&mut colorscheme);
        colorscheme
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
        let colorscheme = colorscheme_with(|colorscheme| {
            colorscheme.cursor = "#010203".to_string();
            colorscheme.success = "#0a0b0c".to_string();
        });

        let palette = Palette::from_colorscheme(&colorscheme);

        assert_eq!(palette.cursor, Color::Rgb(1, 2, 3));
        assert_eq!(palette.success, Color::Rgb(10, 11, 12));
    }

    #[test]
    fn built_in_palette_matches_the_default_colorscheme_strings() {
        // The two representations of Rosé Pine Moon — the hex strings the
        // config layer hands users and the `Color`s the renderer falls back to
        // — are derived from one constant; this fails if a future edit
        // reintroduces a second copy of either.
        let from_strings = Palette::from_colorscheme(&ColorSchemeConfig::default());

        assert_eq!(
            from_strings,
            Palette {
                nc: moon::NC,
                base: moon::BASE,
                muted: moon::MUTED,
                text: moon::TEXT,
                love: moon::LOVE,
                gold: moon::GOLD,
                pine: moon::PINE,
                foam: moon::FOAM,
                iris: moon::IRIS,
                highlight_med: moon::HIGHLIGHT_MED,
                cursor: moon::CURSOR,
                success: moon::SUCCESS,
                monochrome: false,
            }
        );
        // ...and every default parses, so no key is silently on a fallback.
        assert_eq!(
            Palette::from_colorscheme_reporting(&ColorSchemeConfig::default()).1,
            Vec::new()
        );
    }

    #[test]
    fn unparseable_colors_keep_the_default_and_are_reported_per_key() {
        let colorscheme = colorscheme_with(|colorscheme| {
            colorscheme.nc = "not-a-color".to_string();
            colorscheme.gold = "#12345".to_string();
        });

        let (palette, issues) = Palette::from_colorscheme_reporting(&colorscheme);

        assert_eq!(palette.nc, moon::NC);
        assert_eq!(palette.gold, moon::GOLD);
        assert_eq!(
            issues,
            vec![
                ColorParseIssue {
                    key: "_nc",
                    value: "not-a-color".to_string(),
                },
                ColorParseIssue {
                    key: "gold",
                    value: "#12345".to_string(),
                },
            ]
        );
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
