//! Where each surface goes in a frame.
//!
//! Geometry used to live in the renderer, so the event loop had to ask the
//! *rendering* module where a pane was in order to size a PTY or hit-test a
//! click, and the answer was recomputed several times per iteration (F6). It
//! is one value now: [`AppLayout::compute`] is called once per iteration, and
//! both `ui` and the loop's resize/mouse handlers consume the rects it holds.
//!
//! The layout depends on the whole of `App`, not just the terminal size: an
//! open prompt and every live notice take rows off the top of the prompt
//! surface, which is why a notice pushed mid-iteration must be visible before
//! the layout is resolved.

use ratatui::layout::{Constraint, Layout, Rect};

use crate::{
    app::{App, NavItem, OpenWorkspaceMode, Prompt},
    model::{ChatId, TerminalId},
};

/// Columns reserved for the sidebar.
const SIDEBAR_WIDTH: u16 = 34;
/// The main pane never gives up more than this to the sidebar.
const MIN_MAIN_WIDTH: u16 = 40;

/// The frame, divided.
///
/// `Copy` and four rects wide: cheap enough to pass by value everywhere, so no
/// caller is tempted to recompute it because threading a reference was awkward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppLayout {
    /// The frame this layout was computed for. Callers that paint into a frame
    /// compare against it before trusting the rects below: `ratatui` resizes
    /// the buffer inside `Terminal::draw`, so the tick after a host-terminal
    /// resize can hand a layout that was computed for the previous size.
    pub area: Rect,
    pub sidebar: Rect,
    pub main: Rect,
    pub prompt: Rect,
}

impl AppLayout {
    pub fn compute(app: &App, area: Rect) -> Self {
        let [body, prompt] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(prompt_height(app))])
                .areas(area);
        let [sidebar, main] = Layout::horizontal([
            Constraint::Length(SIDEBAR_WIDTH),
            Constraint::Min(MIN_MAIN_WIDTH),
        ])
        .areas(body);

        Self {
            area,
            sidebar,
            main,
            prompt,
        }
    }

    /// Terminal and chat panes are drawn with neither a border nor a header, so
    /// a pane's output area is its whole area. Until F17 this went through a
    /// `pane_inner` / `output_area_after_header` pair whose two header
    /// constants were both `0`.
    pub fn terminal_output(self) -> Rect {
        self.main
    }

    pub fn chat_agent_output(self) -> Rect {
        self.main
    }

    pub fn selected_terminal_output(self, app: &App) -> Option<(TerminalId, Rect)> {
        let Some(NavItem::Terminal { terminal, .. }) = app.selected_item() else {
            return None;
        };

        Some((terminal, self.terminal_output()))
    }

    pub fn selected_chat_agent_output(self, app: &App) -> Option<(ChatId, Rect)> {
        let Some(NavItem::Chat { chat, .. }) = app.selected_item() else {
            return None;
        };

        Some((chat, self.chat_agent_output()))
    }
}

/// Rows the prompt surface takes off the bottom of the frame.
///
/// The status surface only exists while it has something to say, so a quiet
/// session gives every row back to the panes (E2).
fn prompt_height(app: &App) -> u16 {
    let prompt_height = match app.prompt() {
        Some(Prompt::CommandPalette(_)) => 7,
        Some(Prompt::OpenWorkspace(prompt))
            if prompt.mode == OpenWorkspaceMode::ConfiguredProjects =>
        {
            7
        }
        Some(_) => 3,
        None => 0,
    };
    let notice_height = u16::try_from(app.notices().len()).unwrap_or(u16::MAX);
    prompt_height + notice_height
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{NoticeLevel, NoticeSource};

    #[test]
    fn a_notice_takes_a_row_off_the_main_pane() {
        let mut app = App::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let before = AppLayout::compute(&app, frame_area);

        app.push_notice(NoticeLevel::Error, NoticeSource::Report, "boom");
        let after = AppLayout::compute(&app, frame_area);

        assert_eq!(
            after.main.height,
            before.main.height - 1,
            "a notice must cost the main pane exactly one row"
        );
        assert_eq!(after.prompt.height, before.prompt.height + 1);
        assert_eq!(after.area, frame_area);
    }

    #[test]
    fn an_open_prompt_and_its_notices_both_take_rows() {
        let mut app = App::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let quiet = AppLayout::compute(&app, frame_area);

        app.begin_command_palette();
        let prompting = AppLayout::compute(&app, frame_area);
        assert_eq!(prompting.prompt.height, quiet.prompt.height + 7);

        app.push_notice(NoticeLevel::Warning, NoticeSource::Report, "careful");
        let both = AppLayout::compute(&app, frame_area);
        assert_eq!(both.prompt.height, prompting.prompt.height + 1);
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

        let (_, area) = AppLayout::compute(&app, Rect::new(0, 0, 120, 40))
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");

        assert_eq!(area.x, 34);
        assert_eq!(area.y, 0);
        assert_eq!(area.width, 86);
        assert_eq!(area.height, 40);
    }

    #[test]
    fn terminal_output_area_for_tracks_visible_main_pane_without_terminal_selection() {
        let app = App::default();

        let area = AppLayout::compute(&app, Rect::new(0, 0, 120, 40)).terminal_output();

        assert_eq!(area.x, 34);
        assert_eq!(area.y, 0);
        assert_eq!(area.width, 86);
        assert_eq!(area.height, 40);
    }

    #[test]
    fn selected_terminal_output_area_is_absent_for_non_terminal_selection() {
        let app = App::seeded();

        assert_eq!(
            AppLayout::compute(&app, Rect::new(0, 0, 120, 40)).selected_terminal_output(&app),
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
            .selected_chat_agent_output(&app)
            .expect("chat selection has pi output area");

        assert_eq!(area.x, 34);
        assert_eq!(area.y, 0);
        assert_eq!(area.width, 86);
        assert_eq!(area.height, 40);
    }
}
