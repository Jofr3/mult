//! Mouse hit-testing: which pane a click or wheel notch landed in, whether the
//! program running there wants the pointer, and — when it does not — text
//! selection and scrollback.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::layout::AppLayout;
use crate::{
    app::{App, HeldPaneClick, SelectionCell},
    config::Config,
    model::PtyKey,
    pty::{PtyMouseAction, PtyMouseButton, PtyMouseReport, PtyRuntime},
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

    // A program that has grabbed the mouse owns the pointer: clicking in it
    // must reach *it*, not start a selection over a screen it is repainting.
    // Shift is the escape hatch every xterm-descended terminal offers for
    // reaching the terminal's own selection anyway, so it is what returns the
    // pointer to `mult`.
    if !mouse.modifiers.contains(KeyModifiers::SHIFT)
        && forward_mouse_to_program(app, pty_runtime, layout, mouse)
    {
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // A press held over some pane is not what this one is: whatever it
            // was there, it is over.
            flush_held_pane_click(app, pty_runtime);
            begin_text_selection_at_mouse(app, layout, mouse);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            update_text_selection_at_mouse(app, layout, mouse);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            finish_text_selection_at_mouse(app, pty_runtime, config, layout, mouse);
            // The button is up with a press still held, which means the gesture
            // left the pane before it ever became a drag there. It was a click:
            // deliver it now rather than leaving it to be resolved by whatever
            // the pointer does next.
            flush_held_pane_click(app, pty_runtime);
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

/// Hand one mouse event to the program in the pane under the pointer, if it
/// asked for the mouse. Returns whether the event was consumed.
///
/// "Consumed" is decided by the program's mode, not by ours — with one
/// exception, which is the whole of [`HeldPaneClick`]. An event a narrower mode
/// does not want is normally dropped rather than falling through to local
/// selection, because starting a selection over an alternate screen the program
/// is repainting is not what the click meant either. That reasoning holds for a
/// program tracking motion (1002/1003): it sees the drag and is entitled to
/// decide the pointer means nothing there. It does not hold for a program that
/// never asked for motion at all, which is what Claude Code does — DECSET 1000
/// is clicks and releases, no drags. Such a program cannot act on a drag under
/// any interpretation, so dropping it left highlight-to-copy dead over its pane
/// with no way back: the conventional Shift override is eaten by the host
/// terminal (foot, xterm, kitty — all of them force their *own* selection on
/// Shift and never forward the event), so the pointer had nowhere to go.
///
/// So under a no-motion mode the left button is routed by gesture: the press is
/// held, a drag claims it for our selection, and a release without a drag
/// replays press and release together as the click it turned out to be.
fn forward_mouse_to_program(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    layout: AppLayout,
    mouse: MouseEvent,
) -> bool {
    let Some((terminal, area)) = output_terminal_at(app, layout, mouse.column, mouse.row) else {
        return false;
    };
    // `is_running` as well as the mode, because a pane keeps the screen — and
    // therefore the mode — of the program that exited in it. Nobody is there to
    // receive the click, and the last thing a user wants over a dead pane's
    // final screen is a pointer that no longer selects text.
    if !pty_runtime.is_running(terminal) || !pty_runtime.terminal_reports_mouse(terminal) {
        return false;
    }
    let Some(cell) = mouse_cell_in_area(area, mouse.column, mouse.row) else {
        return true;
    };
    let action = pty_mouse_action(mouse.kind);

    // A press held for another pane cannot be resolved by a gesture that has
    // moved on, and it must not be silently discarded either: whatever it was,
    // it was a click there.
    if app
        .held_pane_click()
        .is_some_and(|held| held.terminal != terminal)
    {
        flush_held_pane_click(app, pty_runtime);
    }

    if !pty_runtime
        .terminal_wants_mouse_action(terminal, PtyMouseAction::Drag(PtyMouseButton::Left))
    {
        match action {
            PtyMouseAction::Press(PtyMouseButton::Left) => {
                // A leftover highlight from before the program grabbed the
                // mouse would otherwise sit on top of a screen it is driving.
                app.clear_text_selection();
                app.hold_pane_click(HeldPaneClick {
                    terminal,
                    cell,
                    shift: mouse.modifiers.contains(KeyModifiers::SHIFT),
                    alt: mouse.modifiers.contains(KeyModifiers::ALT),
                    ctrl: mouse.modifiers.contains(KeyModifiers::CONTROL),
                });
                return true;
            }
            PtyMouseAction::Drag(PtyMouseButton::Left) => {
                // The gesture is ours. The program is told nothing — not even
                // the press, which never left.
                if let Some(held) = app.take_held_pane_click(terminal) {
                    app.begin_text_selection(terminal, held.cell);
                }
                // Falls through to the local `Drag` arm, which extends the
                // selection to `cell`.
                return false;
            }
            PtyMouseAction::Release(PtyMouseButton::Left) => {
                let Some(held) = app.take_held_pane_click(terminal) else {
                    // The press became a selection on the first drag, so this
                    // release is ours too — and the program is owed nothing,
                    // having never been told the button went down.
                    return false;
                };
                if held.cell != cell {
                    // A drag whose intermediate motion the host never reported
                    // is still a drag; the local `Up` arm copies it.
                    app.begin_text_selection(terminal, held.cell);
                    return false;
                }
                // Pressed and released on one cell: a click after all, and the
                // program gets the gesture whole and in order.
                pty_runtime.forward_mouse(
                    terminal,
                    held_click_report(PtyMouseAction::Press(PtyMouseButton::Left), held),
                );
                pty_runtime.forward_mouse(
                    terminal,
                    held_click_report(PtyMouseAction::Release(PtyMouseButton::Left), held),
                );
                return true;
            }
            // Anything else the program actually wants — a wheel notch, another
            // button — must not overtake a press being held on its behalf.
            _ if pty_runtime.terminal_wants_mouse_action(terminal, action) => {
                flush_held_pane_click(app, pty_runtime);
            }
            _ => {}
        }
    }

    // A leftover highlight from before the program grabbed the mouse would
    // otherwise sit on top of a screen it is now driving.
    if matches!(action, PtyMouseAction::Press(_)) {
        app.clear_text_selection();
    }

    pty_runtime.forward_mouse(
        terminal,
        pane_mouse_report(
            action,
            cell,
            mouse.modifiers.contains(KeyModifiers::SHIFT),
            mouse.modifiers.contains(KeyModifiers::ALT),
            mouse.modifiers.contains(KeyModifiers::CONTROL),
        ),
    );
    true
}

/// Send a held press on its way as the click it was.
///
/// Press *and* release, always: a press delivered alone would leave the program
/// believing the button is still down, which is exactly the state holding it
/// back was meant to avoid. Under X10, which has no releases, the second write
/// encodes to nothing and the program correctly hears only the press.
fn flush_held_pane_click(app: &mut App, pty_runtime: &mut PtyRuntime) {
    let Some(held) = app.take_any_held_pane_click() else {
        return;
    };
    pty_runtime.forward_mouse(
        held.terminal,
        held_click_report(PtyMouseAction::Press(PtyMouseButton::Left), held),
    );
    pty_runtime.forward_mouse(
        held.terminal,
        held_click_report(PtyMouseAction::Release(PtyMouseButton::Left), held),
    );
}

/// One event addressed to a pane's own screen, in that screen's coordinates.
///
/// The wire is 1-based, and a negative row cannot occur: the cell came from
/// hit-testing the pointer inside the visible pane.
fn pane_mouse_report(
    action: PtyMouseAction,
    cell: SelectionCell,
    shift: bool,
    alt: bool,
    ctrl: bool,
) -> PtyMouseReport {
    PtyMouseReport {
        action,
        col: cell.col.saturating_add(1),
        row: u16::try_from(cell.row).unwrap_or(0).saturating_add(1),
        shift,
        alt,
        ctrl,
    }
}

/// A held press, or the release that completes it, carrying the modifiers that
/// were down when the button went down rather than whatever is down now.
fn held_click_report(action: PtyMouseAction, held: HeldPaneClick) -> PtyMouseReport {
    pane_mouse_report(action, held.cell, held.shift, held.alt, held.ctrl)
}

fn pty_mouse_action(kind: MouseEventKind) -> PtyMouseAction {
    match kind {
        MouseEventKind::Down(button) => PtyMouseAction::Press(pty_mouse_button(button)),
        MouseEventKind::Up(button) => PtyMouseAction::Release(pty_mouse_button(button)),
        MouseEventKind::Drag(button) => PtyMouseAction::Drag(pty_mouse_button(button)),
        MouseEventKind::Moved => PtyMouseAction::Move,
        MouseEventKind::ScrollUp => PtyMouseAction::Press(PtyMouseButton::WheelUp),
        MouseEventKind::ScrollDown => PtyMouseAction::Press(PtyMouseButton::WheelDown),
        MouseEventKind::ScrollLeft => PtyMouseAction::Press(PtyMouseButton::WheelLeft),
        MouseEventKind::ScrollRight => PtyMouseAction::Press(PtyMouseButton::WheelRight),
    }
}

fn pty_mouse_button(button: MouseButton) -> PtyMouseButton {
    match button {
        MouseButton::Left => PtyMouseButton::Left,
        MouseButton::Middle => PtyMouseButton::Middle,
        MouseButton::Right => PtyMouseButton::Right,
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
    // A program that has grabbed the mouse (Claude Code, nvim, less, ...)
    // scrolls its own view, and `handle_mouse` has already handed it the notch;
    // this path is only reached when no program wants the pointer, or when
    // Shift asked for our own scrollback explicitly.
    let Some((terminal, _)) = output_terminal_at(app, layout, mouse.column, mouse.row) else {
        return false;
    };

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
    use crate::app::NavItem;
    use crate::pty::PtyDimensions;
    use crate::runtime::session::restore_persisted_sessions;
    use crate::runtime::{input::handle_event, test_support::*};
    use crate::storage;
    use crossterm::event::Event;
    use crossterm::event::KeyModifiers;
    use mult_protocol::ClientMessage;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;

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

    /// A session whose pane is genuinely attached to a (fake) daemon, with a
    /// program in it that has grabbed the mouse.
    ///
    /// The routing decision is gated on the pane being *live* — a program that
    /// has exited cannot receive a click, however its last screen was left — so
    /// these cases cannot be built on an offline runtime.
    struct GrabbedMouseSession {
        app: App,
        terminal: PtyKey,
        pty_runtime: PtyRuntime,
        config: Config,
        layout: AppLayout,
        output_area: Rect,
        store: storage::StateStore,
        observed: mpsc::Receiver<ClientMessage>,
        server: thread::JoinHandle<()>,
        socket_path: PathBuf,
    }

    impl GrabbedMouseSession {
        /// A pane whose program tracks button motion (DECSET 1002): it owns the
        /// whole pointer, drags included.
        fn new(label: &str, screen: PtyDimensions) -> Self {
            Self::with_mouse_modes(label, screen, b"\x1b[?1002h\x1b[?1006h")
        }

        /// A pane whose program asked for clicks only (DECSET 1000) — what
        /// Claude Code enables. It hears presses and releases and has no way to
        /// interpret a drag.
        fn clicks_only(label: &str, screen: PtyDimensions) -> Self {
            Self::with_mouse_modes(label, screen, b"\x1b[?1000h\x1b[?1006h")
        }

        fn with_mouse_modes(label: &str, screen: PtyDimensions, modes: &[u8]) -> Self {
            let store = test_state_store(label);
            let (mut app, _, terminal) = running_command_app("echo grabbed".to_string());
            let (mut pty_runtime, observed, server, socket_path) =
                recording_attached_runtime(terminal);
            let config = config_with(|config| config.mouse_capture = true);
            let layout = AppLayout::compute(&app, Rect::new(0, 0, 120, 40));
            restore_persisted_sessions(&mut app, &mut pty_runtime, &config, layout);

            app.select_item(NavItem::Terminal {
                workspace: app.project.workspaces[0].id,
                terminal,
            });
            let terminal = PtyKey::Terminal(terminal);
            assert!(
                pty_runtime.is_running(terminal),
                "the fixture pane must be attached"
            );

            pty_runtime
                .resize(terminal, screen)
                .expect("size the pane's screen");
            pty_runtime.process_terminal_output(terminal, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
            // The program turns on SGR mouse reporting: from here the pointer is
            // its input, as far as the mode it asked for reaches.
            pty_runtime.process_terminal_output(terminal, modes);

            let (_, output_area) = layout
                .selected_terminal_output(&app)
                .expect("terminal selection has output area");
            Self {
                app,
                terminal,
                pty_runtime,
                config,
                layout,
                output_area,
                store,
                observed,
                server,
                socket_path,
            }
        }

        fn mouse(&mut self, kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) {
            handle_event(
                &mut self.app,
                &mut self.pty_runtime,
                &self.config,
                &self.store,
                Event::Mouse(MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers,
                }),
                self.layout,
            );
        }

        fn scrollback(&self) -> usize {
            self.pty_runtime
                .parser(self.terminal)
                .expect("the pane has a parser")
                .screen()
                .scrollback()
        }

        /// Every `Input` payload the client wrote, in order. Consumes the
        /// session: the recording server only stops once the socket is dropped.
        fn recorded_input(self) -> Vec<Vec<u8>> {
            drop(self.pty_runtime);
            self.server.join().expect("recording server exits");
            let _ = fs::remove_file(&self.socket_path);
            self.observed
                .into_iter()
                .filter_map(|message| match message {
                    ClientMessage::Input { bytes, .. } => Some(bytes),
                    _ => None,
                })
                .collect()
        }
    }

    #[test]
    fn a_grabbed_pointer_reaches_the_program_and_leaves_our_scrollback_alone() {
        let mut session = GrabbedMouseSession::new(
            "grabbed-pointer-reaches-the-program",
            PtyDimensions { rows: 24, cols: 80 },
        );
        let area = session.output_area;

        for (kind, column, row) in [
            (MouseEventKind::ScrollUp, area.x, area.y),
            (
                MouseEventKind::Down(MouseButton::Left),
                area.x + 2,
                area.y + 1,
            ),
            (
                MouseEventKind::Drag(MouseButton::Left),
                area.x + 4,
                area.y + 1,
            ),
            (
                MouseEventKind::Up(MouseButton::Left),
                area.x + 4,
                area.y + 1,
            ),
            // 1002 was requested, not 1003, so this one is dropped rather than
            // written.
            (MouseEventKind::Moved, area.x + 5, area.y + 1),
        ] {
            session.mouse(kind, column, row, KeyModifiers::NONE);
        }

        // The wheel notch went to the program, so our own view never moved...
        assert_eq!(session.scrollback(), 0);
        // ...and the click never became a local selection.
        assert_eq!(session.app.text_selection_for(session.terminal), None);

        assert_eq!(
            session.recorded_input(),
            vec![
                b"\x1b[<64;1;1M".to_vec(),
                b"\x1b[<0;3;2M".to_vec(),
                b"\x1b[<32;5;2M".to_vec(),
                b"\x1b[<0;5;2m".to_vec(),
            ],
            "press, drag and release reach the program; a bare move does not",
        );
    }

    #[test]
    fn a_drag_over_a_clicks_only_program_selects_locally_and_tells_it_nothing() {
        let mut session = GrabbedMouseSession::clicks_only(
            "drag-over-a-clicks-only-program",
            PtyDimensions { rows: 24, cols: 80 },
        );
        let area = session.output_area;

        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), area.x + 1),
            (MouseEventKind::Drag(MouseButton::Left), area.x + 3),
            (MouseEventKind::Up(MouseButton::Left), area.x + 5),
        ] {
            session.mouse(kind, column, area.y, KeyModifiers::NONE);
        }

        let selection = session
            .app
            .text_selection_for(session.terminal)
            .expect("a plain drag selects over a program that never asked for motion");
        assert_eq!(
            selection.anchor.col, 1,
            "the selection starts where the button went down, not where the drag was first reported"
        );
        assert_eq!(selection.focus.col, 5);

        assert!(
            session.recorded_input().is_empty(),
            "the program hears nothing: not the drag it cannot read, and not the press that became one",
        );
    }

    #[test]
    fn a_click_over_a_clicks_only_program_still_reaches_it_whole() {
        let mut session = GrabbedMouseSession::clicks_only(
            "click-over-a-clicks-only-program",
            PtyDimensions { rows: 24, cols: 80 },
        );
        let area = session.output_area;

        session.mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 2,
            area.y + 1,
            KeyModifiers::NONE,
        );
        // Held, not dropped: at this point the gesture is equally a click and
        // the first frame of a selection.
        assert_eq!(session.app.text_selection_for(session.terminal), None);

        session.mouse(
            MouseEventKind::Up(MouseButton::Left),
            area.x + 2,
            area.y + 1,
            KeyModifiers::NONE,
        );

        assert_eq!(
            session.app.text_selection_for(session.terminal),
            None,
            "a click is not a selection"
        );
        assert_eq!(
            session.recorded_input(),
            vec![b"\x1b[<0;3;2M".to_vec(), b"\x1b[<0;3;2m".to_vec()],
            "press and release arrive together and in order, at the cell pressed",
        );
    }

    #[test]
    fn a_wheel_notch_never_overtakes_a_press_held_for_the_program() {
        let mut session = GrabbedMouseSession::clicks_only(
            "wheel-notch-never-overtakes-a-held-press",
            // Short enough that the fixture's five lines leave scrollback to
            // move, had the notch been ours.
            PtyDimensions { rows: 2, cols: 8 },
        );
        let area = session.output_area;

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            // A wheel notch is a press under every mode, so this one is the
            // program's — and it must not arrive before the press it followed.
            MouseEventKind::ScrollUp,
            MouseEventKind::Up(MouseButton::Left),
        ] {
            session.mouse(kind, area.x + 1, area.y, KeyModifiers::NONE);
        }

        assert_eq!(
            session.scrollback(),
            0,
            "the notch went to the program, not to our own view"
        );
        assert_eq!(
            session.recorded_input(),
            vec![
                b"\x1b[<0;2;1M".to_vec(),
                b"\x1b[<0;2;1m".to_vec(),
                b"\x1b[<64;2;1M".to_vec(),
            ],
            "the held press is completed as a click before the notch that followed it",
        );
    }

    #[test]
    fn a_dead_pane_gives_the_pointer_back_even_with_the_mouse_mode_still_set() {
        // A program killed mid-run never emits the resets its exit path would
        // have, so its pane keeps reporting the mouse. Nobody is there to read
        // a click, and the user still wants to select the text it left behind.
        let store = test_state_store("dead-pane-gives-the-pointer-back");
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
            .resize(terminal_id, PtyDimensions { rows: 2, cols: 8 })
            .expect("resize parser");
        pty_runtime.process_terminal_output(terminal_id, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        pty_runtime.process_terminal_output(terminal_id, b"\x1b[?1002h\x1b[?1006h");
        assert!(pty_runtime.terminal_reports_mouse(terminal_id));
        assert!(!pty_runtime.is_running(terminal_id));
        let config = config_with(|config| config.mouse_capture = true);
        let layout = AppLayout::compute(&app, Rect::new(0, 0, 120, 40));
        let (_, output_area) = layout
            .selected_terminal_output(&app)
            .expect("terminal selection has output area");

        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), output_area.x),
            (MouseEventKind::Drag(MouseButton::Left), output_area.x + 2),
        ] {
            handle_event(
                &mut app,
                &mut pty_runtime,
                &config,
                &store,
                Event::Mouse(MouseEvent {
                    kind,
                    column,
                    row: output_area.y,
                    modifiers: KeyModifiers::NONE,
                }),
                layout,
            );
        }

        let selection = app
            .text_selection_for(terminal_id)
            .expect("a dead pane's screen is still selectable");
        assert_eq!(selection.focus.col, 2);
    }

    #[test]
    fn shift_takes_the_pointer_back_from_a_mouse_reporting_program() {
        let mut session = GrabbedMouseSession::new(
            "shift-takes-the-pointer-back",
            // A short screen, so the fixture's five lines leave scrollback.
            PtyDimensions { rows: 2, cols: 8 },
        );
        let area = session.output_area;

        // Shift is the conventional way to reach the terminal's own selection
        // over a program that has grabbed the mouse.
        for (kind, column) in [
            (MouseEventKind::Down(MouseButton::Left), area.x),
            (MouseEventKind::Drag(MouseButton::Left), area.x + 2),
        ] {
            session.mouse(kind, column, area.y, KeyModifiers::SHIFT);
        }

        let selection = session
            .app
            .text_selection_for(session.terminal)
            .expect("Shift+drag selects locally even under a mouse grab");
        assert_eq!(selection.anchor.col, 0);
        assert_eq!(selection.focus.col, 2);

        // ...and Shift+wheel still scrolls our own buffer rather than the
        // program's view.
        session.mouse(
            MouseEventKind::ScrollUp,
            area.x,
            area.y,
            KeyModifiers::SHIFT,
        );
        assert_eq!(session.scrollback(), 3);

        // Nothing at all was written to the program: the whole gesture was ours.
        assert!(session.recorded_input().is_empty());
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
