//! Where the frame's panes are.
//!
//! This used to live in `ui`, which made the renderer the geometry oracle as
//! well as a `State → Frame` function: the event loop asked the *rendering*
//! module where the selected pane was, and `layout_areas` was recomputed at
//! least four times per iteration — once for the PTY size, once for the chat
//! size, once for the selected output area and once more for mouse hit-testing
//! (F6). It is computed once per iteration now, and the same value is handed to
//! `ui::draw` and to the resize and mouse handlers, so the geometry the user
//! clicks on and the geometry the PTY is sized to cannot disagree.

use ratatui::layout::{Constraint, Layout, Rect};

use crate::{
    app::{App, NavItem, OpenWorkspaceMode, Prompt},
    model::PtyKey,
};

/// Commands listed in full by the restore prompt before it starts eliding.
///
/// It lives here because the prompt's *height* depends on it, and the height is
/// layout; `ui::prompt` reads the same constant when it draws the rows.
pub const MAX_LISTED_RESTORE_COMMANDS: usize = 5;

/// The fixed part of the layout: what the frame is divided into before the
/// frame's own size is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneLayout {
    sidebar_width: u16,
    min_main_width: u16,
    status_height: u16,
    prompt_height: u16,
}

/// Every rect a frame is drawn into, resolved for one frame size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppLayout {
    /// The whole frame, so an overlay can be centred on it without asking the
    /// terminal again.
    pub frame: Rect,
    pub sidebar: Rect,
    pub main: Rect,
    /// The global status line. Zero-height when there is nothing to say.
    pub status: Rect,
    /// The prompt row(s). Zero-height when no prompt is open.
    pub prompt: Rect,
    /// Where a terminal's output goes, whether or not a terminal is selected —
    /// a terminal is started at the size the pane *would* have.
    pub terminal_output: Rect,
    /// The same, for a chat's agent PTY.
    pub chat_output: Rect,
    /// The selected pane's PTY and the area its output occupies, or `None` when
    /// nothing is selected. This is the one the mouse hit-tests against and the
    /// one a visible PTY is resized to.
    pub selected_output: Option<(PtyKey, Rect)>,
}

impl AppLayout {
    /// Resolve the layout for `frame_area`. Cheap and pure: two ratatui splits
    /// plus a look at what is selected and what the prompt is.
    pub fn compute(app: &App, frame_area: Rect) -> Self {
        let panes = PaneLayout::for_app(app);
        let [body, status, prompt] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(panes.status_height),
            Constraint::Length(panes.prompt_height),
        ])
        .areas(frame_area);
        let [sidebar, main] = Layout::horizontal([
            Constraint::Length(panes.sidebar_width),
            Constraint::Min(panes.min_main_width),
        ])
        .areas(body);

        let output = output_area(main);
        let selected_output = match app.selected_item() {
            Some(NavItem::Terminal { terminal, .. }) => Some((PtyKey::Terminal(terminal), output)),
            Some(NavItem::Chat { chat, .. }) => Some((PtyKey::ChatAgent(chat), output)),
            None => None,
        };

        Self {
            frame: frame_area,
            sidebar,
            main,
            status,
            prompt,
            terminal_output: output,
            chat_output: output,
            selected_output,
        }
    }

    /// The selected terminal and its output area, when a terminal is selected.
    pub fn selected_terminal_output(&self) -> Option<(crate::model::TerminalId, Rect)> {
        match self.selected_output {
            Some((PtyKey::Terminal(terminal), area)) => Some((terminal, area)),
            _ => None,
        }
    }

    /// The selected chat's agent PTY and its output area, when a chat is
    /// selected.
    pub fn selected_chat_agent_output(&self) -> Option<(crate::model::ChatId, Rect)> {
        match self.selected_output {
            Some((PtyKey::ChatAgent(chat), area)) => Some((chat, area)),
            _ => None,
        }
    }
}

impl PaneLayout {
    fn for_app(app: &App) -> Self {
        Self {
            sidebar_width: 34,
            min_main_width: 40,
            status_height: status_height(app),
            prompt_height: prompt_height(app),
        }
    }
}

/// One row while a notice is undismissed, none otherwise.
///
/// Zero-height when there is nothing to say is the whole point: the status
/// surface must be able to report a failed daemon connection without costing a
/// row of terminal output for the rest of the session (E2).
fn status_height(app: &App) -> u16 {
    u16::from(app.current_status_notice().is_some())
}

fn prompt_height(app: &App) -> u16 {
    match app.prompt() {
        Some(Prompt::CommandPalette(_)) => 7,
        Some(Prompt::OpenWorkspace(prompt))
            if prompt.mode == OpenWorkspaceMode::ConfiguredProjects =>
        {
            7
        }
        // One extra row when the delete also removes the workspace, so the
        // warning is never the line that gets cut off.
        Some(Prompt::ConfirmDelete(prompt)) => 3 + u16::from(prompt.cascade.is_some()),
        // Header, one row per listed command (plus an elision row), hint.
        Some(Prompt::ConfirmRestore(prompt)) => {
            let listed = prompt.entries.len().min(MAX_LISTED_RESTORE_COMMANDS);
            let elided = usize::from(prompt.entries.len() > MAX_LISTED_RESTORE_COMMANDS);
            2 + u16::try_from(listed + elided).unwrap_or(u16::MAX)
        }
        // The overlay is drawn over the frame and costs the prompt row nothing.
        Some(Prompt::Help(_)) => 0,
        Some(_) => 3,
        None => 0,
    }
}

/// The area a pane's output occupies. Both pane kinds draw straight into the
/// main area: the header rows they used to reserve are gone, and the two
/// constants that expressed that were both `0` (F17).
pub fn output_area(main: Rect) -> Rect {
    main
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_line_takes_no_space_until_there_is_something_to_say() {
        let mut app = App::two_workspaces();
        let frame_area = Rect::new(0, 0, 100, 12);

        let quiet = AppLayout::compute(&app, frame_area);
        app.set_last_error("boom");
        let noisy = AppLayout::compute(&app, frame_area);

        assert_eq!(quiet.status.height, 0);
        assert_eq!(noisy.status.height, 1);
        assert_eq!(noisy.main.height, quiet.main.height - 1);
        assert_eq!(noisy.status.y, quiet.main.bottom() - 1);
    }

    #[test]
    fn selected_terminal_output_area_tracks_visible_main_pane_size() {
        let mut app = App::two_workspaces();
        let selected = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Terminal { .. }))
            .expect("seed state has a terminal");
        app.select_nav_index(selected);

        let (_, area) = AppLayout::compute(&app, Rect::new(0, 0, 120, 40))
            .selected_terminal_output()
            .expect("terminal selection has output area");

        assert_eq!(area.x, 34);
        assert_eq!(area.y, 0);
        assert_eq!(area.width, 86);
        assert_eq!(area.height, 40);
    }

    #[test]
    fn terminal_output_area_tracks_visible_main_pane_without_terminal_selection() {
        let app = App::two_workspaces();

        let area = AppLayout::compute(&app, Rect::new(0, 0, 120, 40)).terminal_output;

        assert_eq!(area.x, 34);
        assert_eq!(area.y, 0);
        assert_eq!(area.width, 86);
        assert_eq!(area.height, 40);
    }

    #[test]
    fn selected_terminal_output_area_is_absent_for_non_terminal_selection() {
        let app = App::seeded();

        assert_eq!(
            AppLayout::compute(&app, Rect::new(0, 0, 120, 40)).selected_terminal_output(),
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

        let (_, area) = AppLayout::compute(&app, Rect::new(0, 0, 120, 40))
            .selected_chat_agent_output()
            .expect("chat selection has pi output area");

        assert_eq!(area.x, 34);
        assert_eq!(area.y, 0);
        assert_eq!(area.width, 86);
        assert_eq!(area.height, 40);
    }
}
