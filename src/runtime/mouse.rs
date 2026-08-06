//! Mouse hit-testing: which pane a click or wheel notch landed in, text
//! selection, and scrollback.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::layout::AppLayout;
use crate::{
    app::{App, SelectionCell},
    config::Config,
    model::PtyKey,
    pty::PtyRuntime,
};

use super::clipboard::copy_text_selection_to_clipboard;

const MOUSE_SCROLL_ROWS: usize = 3;

pub(super) fn handle_mouse(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    mouse: MouseEvent,
    layout: AppLayout,
) {
    if app.is_prompt_active() {
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            begin_text_selection_at_mouse(app, layout, mouse);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            update_text_selection_at_mouse(app, layout, mouse);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            finish_text_selection_at_mouse(app, pty_runtime, config, layout, mouse);
        }
        MouseEventKind::ScrollUp => {
            scroll_output_at_mouse(app, pty_runtime, layout, mouse, ScrollDirection::Up);
        }
        MouseEventKind::ScrollDown => {
            scroll_output_at_mouse(app, pty_runtime, layout, mouse, ScrollDirection::Down);
        }
        _ => {}
    }
}

fn begin_text_selection_at_mouse(app: &mut App, layout: AppLayout, mouse: MouseEvent) -> bool {
    let Some((terminal, area)) = selected_output_area(app, layout) else {
        app.clear_text_selection();
        return false;
    };
    if !rect_contains(area, mouse.column, mouse.row) {
        app.clear_text_selection();
        return false;
    }
    let Some(cell) = mouse_cell_in_area(area, mouse.column, mouse.row) else {
        return false;
    };
    app.begin_text_selection(terminal, cell);
    true
}

fn update_text_selection_at_mouse(app: &mut App, layout: AppLayout, mouse: MouseEvent) -> bool {
    let Some((terminal, cell)) = active_selection_cell_at_mouse(app, layout, mouse) else {
        return false;
    };
    app.update_text_selection(terminal, cell)
}

fn finish_text_selection_at_mouse(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: AppLayout,
    mouse: MouseEvent,
) -> bool {
    let Some((terminal, cell)) = active_selection_cell_at_mouse(app, layout, mouse) else {
        return false;
    };
    let Some(selection) = app.end_text_selection(terminal, cell) else {
        return false;
    };
    if selection.anchor == selection.focus {
        app.clear_text_selection();
        return false;
    }
    copy_text_selection_to_clipboard(pty_runtime, config, selection);
    true
}

fn active_selection_cell_at_mouse(
    app: &App,
    layout: AppLayout,
    mouse: MouseEvent,
) -> Option<(PtyKey, SelectionCell)> {
    let selection = app.text_selection?;
    let (terminal, area) = selected_output_area(app, layout)?;
    if terminal != selection.terminal {
        return None;
    }
    mouse_cell_in_area(area, mouse.column, mouse.row).map(|cell| (terminal, cell))
}

fn selected_output_area(app: &App, layout: AppLayout) -> Option<(PtyKey, Rect)> {
    if let Some((terminal, area)) = layout.selected_terminal_output(app) {
        return Some((PtyKey::Terminal(terminal), area));
    }
    layout
        .selected_chat_agent_output(app)
        .map(|(chat, area)| (PtyKey::ChatAgent(chat), area))
}

fn mouse_cell_in_area(area: Rect, column: u16, row: u16) -> Option<SelectionCell> {
    if area.is_empty() {
        return None;
    }
    Some(SelectionCell {
        row: i32::from(
            row.saturating_sub(area.y)
                .min(area.height.saturating_sub(1)),
        ),
        col: column
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(1)),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollDirection {
    Up,
    Down,
}

fn scroll_output_at_mouse(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    layout: AppLayout,
    mouse: MouseEvent,
    direction: ScrollDirection,
) -> bool {
    let Some((terminal, area)) = output_terminal_at(app, layout, mouse.column, mouse.row) else {
        return false;
    };

    // A program that has grabbed the mouse (Claude Code, nvim, less, ...)
    // scrolls its own view. Our local scrollback holds nothing for it — the
    // alternate screen keeps none — so hand the wheel notch to the program
    // instead of swallowing it into a buffer that can never move.
    if pty_runtime.terminal_reports_mouse(terminal) {
        let Some(cell) = mouse_cell_in_area(area, mouse.column, mouse.row) else {
            return false;
        };
        let col = cell.col.saturating_add(1);
        let row = u16::try_from(cell.row).unwrap_or(0).saturating_add(1);
        return pty_runtime.forward_wheel(terminal, direction == ScrollDirection::Up, col, row);
    }

    match direction {
        ScrollDirection::Up => {
            scroll_terminal_output_up(app, pty_runtime, terminal, MOUSE_SCROLL_ROWS)
        }
        ScrollDirection::Down => {
            scroll_terminal_output_down(app, pty_runtime, terminal, MOUSE_SCROLL_ROWS)
        }
    }
}

fn output_terminal_at(
    app: &App,
    layout: AppLayout,
    column: u16,
    row: u16,
) -> Option<(PtyKey, Rect)> {
    selected_output_area(app, layout).filter(|(_, area)| rect_contains(*area, column, row))
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn scroll_terminal_output_up(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    terminal: PtyKey,
    rows: usize,
) -> bool {
    let before = terminal_scrollback(pty_runtime, terminal);
    let changed = pty_runtime.scroll_up(terminal, rows).unwrap_or(false);
    sync_text_selection_with_scrollback(app, pty_runtime, terminal, before, changed);
    changed
}

fn scroll_terminal_output_down(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    terminal: PtyKey,
    rows: usize,
) -> bool {
    let before = terminal_scrollback(pty_runtime, terminal);
    let changed = pty_runtime.scroll_down(terminal, rows).unwrap_or(false);
    sync_text_selection_with_scrollback(app, pty_runtime, terminal, before, changed);
    changed
}

fn terminal_scrollback(pty_runtime: &PtyRuntime, terminal: PtyKey) -> usize {
    pty_runtime
        .parser(terminal)
        .map(|parser| parser.screen().scrollback())
        .unwrap_or_default()
}

fn sync_text_selection_with_scrollback(
    app: &mut App,
    pty_runtime: &PtyRuntime,
    terminal: PtyKey,
    before: usize,
    changed: bool,
) {
    if !changed {
        return;
    }
    let after = terminal_scrollback(pty_runtime, terminal);
    let delta = (after as i64 - before as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    app.shift_text_selection_rows(terminal, delta);
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::pty::PtyDimensions;
    use crate::runtime::{input::handle_event, test_support::*};
    use crossterm::event::Event;
    use crossterm::event::KeyModifiers;

    #[test]
    fn mouse_wheel_scrolls_output_under_cursor() {
        let store = test_state_store("mouse-wheel-scrolls-output-under-cursor");
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                crate::app::NavItem::Terminal { terminal, .. } => {
                    Some((index, PtyKey::Terminal(*terminal)))
                }
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let config = config_with(|config| config.mouse_capture = true);
        let layout = AppLayout::compute(&app, Rect::new(0, 0, 120, 40));
        let (_, output_area) = layout
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            layout,
        );
        assert_eq!(
            pty_runtime.terminal_lines(terminal_id),
            vec!["one".to_string(), "two".to_string()]
        );

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            layout,
        );
        assert_eq!(
            pty_runtime.terminal_lines(terminal_id),
            vec!["four".to_string(), "five".to_string()]
        );
    }

    #[test]
    fn mouse_wheel_does_not_scroll_local_buffer_when_program_grabs_mouse() {
        let store = test_state_store("mouse-wheel-does-not-scroll-local-buffer");
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                crate::app::NavItem::Terminal { terminal, .. } => {
                    Some((index, PtyKey::Terminal(*terminal)))
                }
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        // The program turns on mouse reporting: the wheel is now its input, so
        // our local scrollback must stay pinned to the bottom.
        pty_runtime.process_terminal_output(terminal_id, b"\x1b[?1000h\x1b[?1006h");
        let config = config_with(|config| config.mouse_capture = true);
        let layout = AppLayout::compute(&app, Rect::new(0, 0, 120, 40));
        let (_, output_area) = layout
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            layout,
        );

        assert_eq!(
            pty_runtime
                .parser(terminal_id)
                .unwrap()
                .screen()
                .scrollback(),
            0
        );
        assert_eq!(
            pty_runtime.terminal_lines(terminal_id),
            vec!["four".to_string(), "five".to_string()]
        );
    }

    #[test]
    fn mouse_wheel_scroll_moves_text_selection_with_scrollback() {
        let store = test_state_store("mouse-wheel-scroll-moves-text-selection-");
        let mut app = App::default();
        let (selected, terminal_id) = app
            .nav_items()
            .iter()
            .enumerate()
            .find_map(|(index, item)| match item {
                crate::app::NavItem::Terminal { terminal, .. } => {
                    Some((index, PtyKey::Terminal(*terminal)))
                }
                _ => None,
            })
            .expect("seed state has a terminal");
        app.select_nav_index(selected);
        app.begin_text_selection(terminal_id, SelectionCell { row: 0, col: 0 });
        app.update_text_selection(terminal_id, SelectionCell { row: 0, col: 2 });

        let mut pty_runtime = PtyRuntime::new_offline();
        pty_runtime
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let config = config_with(|config| config.mouse_capture = true);
        let layout = AppLayout::compute(&app, Rect::new(0, 0, 120, 40));
        let (_, output_area) = layout
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            layout,
        );
        let selection = app
            .text_selection_for(terminal_id)
            .expect("selection follows scroll up");
        assert_eq!(selection.anchor.row, 3);
        assert_eq!(selection.focus.row, 3);

        handle_event(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: output_area.x,
                row: output_area.y,
                modifiers: KeyModifiers::NONE,
            }),
            layout,
        );
        let selection = app
            .text_selection_for(terminal_id)
            .expect("selection follows scroll down");
        assert_eq!(selection.anchor.row, 0);
        assert_eq!(selection.focus.row, 0);
    }
}
