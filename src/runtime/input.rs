//! Keyboard and paste dispatch: which surface a key belongs to, and the global
//! control-key shortcuts.
//!
//! The dispatch order is load-bearing. Quit is checked first, then the modal
//! help overlay, then an open prompt, and only then does a key reach the
//! selected pane — so nothing behind a modal surface can see a keystroke.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;

use crate::{
    app::{App, NavItem, Prompt},
    config::Config,
    model::{AgentKind, PtyKey},
    pty::PtyRuntime,
    storage,
};

use super::agent_launch::{
    add_agent_to_selected_workspace, start_or_focus_chat_agent, start_or_focus_selected_chat_agent,
    ChatAgentLaunch,
};
use super::clipboard::copy_current_text_selection;
use super::keymap::{
    is_control_down_key, is_control_up_key, is_quit_key, is_shifted_control_char,
    is_unshifted_control_char, key_to_pty_bytes_in_mode,
};
use super::mouse::handle_mouse;
use super::prompt::{
    handle_command_palette_key, handle_delete_confirmation_key, handle_open_workspace_key,
    handle_search_key, handle_terminal_command_key,
};
use super::session::{start_or_focus_selected_terminal, start_terminal};

pub(super) fn handle_event(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    event: Event,
    frame_area: Rect,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, pty_runtime, config, store, key, frame_area);
        }
        Event::Mouse(mouse) => handle_mouse(app, pty_runtime, config, mouse, frame_area),
        Event::Paste(text) => handle_paste(app, pty_runtime, config, store, text, frame_area),
        _ => {}
    }
}

pub(super) fn handle_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    frame_area: Rect,
) {
    if is_quit_key(key) {
        app.quit();
        return;
    }

    // The overlay is modal: while it is up it owns the keyboard, so no key can
    // reach a PTY behind it. Anything that is not a shortcut closes it.
    if app.is_help_visible() {
        handle_help_overlay_key(app, key);
        return;
    }

    match &app.prompt {
        Some(Prompt::OpenWorkspace(_)) => handle_open_workspace_key(app, config, key),
        Some(Prompt::NewTerminalCommand(_)) => handle_terminal_command_key(app, key),
        Some(Prompt::CommandPalette(_)) => {
            handle_command_palette_key(app, pty_runtime, config, store, key, frame_area);
        }
        Some(Prompt::Search(_)) => handle_search_key(app, key),
        Some(Prompt::ConfirmDelete(_)) => handle_delete_confirmation_key(app, pty_runtime, key),
        None => handle_unprompted_key(app, pty_runtime, config, store, key, frame_area),
    }
}

fn handle_paste(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    text: String,
    frame_area: Rect,
) {
    if app.is_prompt_active() {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            app.push_prompt_char(ch);
        }
        return;
    }

    let Some(terminal_id) =
        start_selected_pty_if_needed(app, pty_runtime, config, store, frame_area)
    else {
        return;
    };

    match pty_runtime.send_paste(terminal_id, &text) {
        Ok(true) => {}
        Ok(false) => {
            pty_runtime.append_terminal_system_line(terminal_id, "PTY is not running");
        }
        Err(error) => {
            pty_runtime
                .append_terminal_system_line(terminal_id, format!("failed to paste: {error}"));
        }
    }
}

fn handle_unprompted_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    frame_area: Rect,
) {
    if opens_help(app, key) {
        app.show_help();
        return;
    }
    if handle_control_key(app, pty_runtime, config, store, key, frame_area) {
        return;
    }

    handle_selected_pty_input_key(app, pty_runtime, config, store, key, frame_area);
}

/// `F1` always opens the overlay; `?` only when no pane would have received it.
///
/// A selected chat or terminal takes every ordinary key — that is how a PTY is
/// started and typed at, there is no input mode to leave — so a global `?`
/// would steal a character from every shell, pager and editor running in a
/// pane. `F1` is safe to take unconditionally: nothing in `mult` sent it
/// anywhere useful before, and it is the one key a full-screen program is
/// unlikely to need. `Ctrl+p` → "Show keybindings" reaches the overlay from
/// anywhere.
fn opens_help(app: &App, key: KeyEvent) -> bool {
    if matches!(key.code, KeyCode::F(1)) {
        return true;
    }
    matches!(key.code, KeyCode::Char('?'))
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && app.help_key_opens_help()
}

/// Any key closes the overlay. It carries no state of its own, so there is
/// nothing to navigate and nothing a stray keystroke can damage — and a user
/// who cannot find the dismissal key is stuck in a modal screen.
fn handle_help_overlay_key(app: &mut App, _key: KeyEvent) {
    app.hide_help();
}

fn handle_control_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    frame_area: Rect,
) -> bool {
    if is_shifted_control_char(key, 'c') {
        copy_current_text_selection(app, pty_runtime, config);
        return true;
    }

    if is_control_down_key(key) {
        app.select_next();
        return true;
    }
    if is_control_up_key(key) {
        app.select_previous();
        return true;
    }
    if is_unshifted_control_char(key, 'q') {
        app.begin_delete_selected();
        return true;
    }
    if is_unshifted_control_char(key, 'p') {
        app.begin_command_palette();
        return true;
    }
    if is_unshifted_control_char(key, 's') && app.begin_search() {
        return true;
    }
    if is_unshifted_control_char(key, 'a') {
        add_agent_to_selected_workspace(app, pty_runtime, config, store, frame_area, AgentKind::Pi);
        return true;
    }
    if is_unshifted_control_char(key, 'x') {
        add_agent_to_selected_workspace(
            app,
            pty_runtime,
            config,
            store,
            frame_area,
            AgentKind::ClaudeCode,
        );
        return true;
    }
    if is_unshifted_control_char(key, 't') {
        app.add_terminal_to_selected_workspace();
        return true;
    }
    if is_unshifted_control_char(key, 'f') {
        app.begin_open_workspace(&config.projects);
        return true;
    }
    // Only consumed when there is something to dismiss, so `Ctrl+n` still
    // reaches a PTY on a quiet session.
    if is_unshifted_control_char(key, 'n') && app.dismiss_notices() {
        return true;
    }

    false
}

fn handle_selected_pty_input_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    frame_area: Rect,
) {
    // Emptiness does not depend on cursor-key mode, so this also avoids starting
    // a PTY for keys that map to nothing (e.g. shortcuts handled elsewhere).
    if key_to_pty_bytes_in_mode(key, false).is_empty() {
        return;
    }

    let Some(terminal_id) =
        start_selected_pty_if_needed(app, pty_runtime, config, store, frame_area)
    else {
        return;
    };

    // Honour the application cursor-key mode (DECCKM) the PTY program requested,
    // so arrows reach full-screen apps in the SS3 form they expect.
    let application_cursor = pty_runtime
        .parser(terminal_id)
        .is_some_and(|parser| parser.screen().application_cursor());
    let bytes = key_to_pty_bytes_in_mode(key, application_cursor);

    match pty_runtime.send_input(terminal_id, &bytes) {
        Ok(true) => {}
        Ok(false) => {
            pty_runtime.append_terminal_system_line(terminal_id, "PTY is not running");
        }
        Err(error) => {
            pty_runtime
                .append_terminal_system_line(terminal_id, format!("failed to send input: {error}"));
        }
    }
}

fn start_selected_pty_if_needed(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    frame_area: Rect,
) -> Option<PtyKey> {
    match app.selected_item()? {
        NavItem::Chat { workspace, chat } => {
            let terminal = PtyKey::ChatAgent(chat);
            if pty_runtime.is_running(terminal) {
                app.begin_chat_agent_input();
            } else {
                start_or_focus_chat_agent(
                    app,
                    pty_runtime,
                    config,
                    store,
                    frame_area,
                    ChatAgentLaunch {
                        workspace_id: workspace,
                        chat_id: chat,
                        focus_after_start: true,
                    },
                );
            }
            pty_runtime.is_running(terminal).then_some(terminal)
        }
        NavItem::Terminal {
            workspace,
            terminal,
        } => {
            let key = PtyKey::Terminal(terminal);
            if !pty_runtime.is_running(key) {
                start_terminal(app, pty_runtime, config, frame_area, workspace, terminal);
            }
            if pty_runtime.is_running(key) {
                app.begin_terminal_input();
                Some(key)
            } else {
                None
            }
        }
    }
}

pub(super) fn focus_selected_input(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    frame_area: Rect,
) {
    if app.selected_chat_id().is_some() {
        start_or_focus_selected_chat_agent(app, pty_runtime, config, store, frame_area);
    } else if app.selected_terminal_id().is_some() {
        start_or_focus_selected_terminal(app, pty_runtime, config, frame_area);
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::app::NoticeLevel;
    use crate::app::NoticeSource;
    use crate::runtime::{keymap::key_to_pty_bytes, test_support::*};

    #[test]
    fn ctrl_j_and_ctrl_k_navigate_selection() {
        let store = test_state_store("ctrl-j-and-ctrl-k-navigate-selection");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(app.selected_index(), Some(1));
        assert_eq!(app.selected_item(), Some(app.nav_items()[1]));

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(app.selected_index(), Some(0));
        assert_eq!(app.selected_item(), Some(app.nav_items()[0]));
    }

    #[test]
    fn ctrl_p_opens_palette_and_ctrl_s_opens_search_for_selected_pane() {
        let store = test_state_store("ctrl-p-opens-palette-and-ctrl-s-opens-se");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::CommandPalette(_))));
        app.cancel_prompt();

        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        let target = NavItem::Terminal {
            workspace,
            terminal,
        };
        app.select_item(target);
        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::Search(_))));
    }

    #[test]
    fn plain_keys_are_not_workspace_commands() {
        let store = test_state_store("plain-keys-are-not-workspace-commands");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            frame_area,
        );
        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            frame_area,
        );

        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
        assert!(!app.should_quit);
        assert_eq!(app.prompt, None);
    }

    #[test]
    fn ctrl_keys_create_delete_and_quit() {
        let store = test_state_store("ctrl-keys-create-delete-and-quit");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(
                KeyCode::Char('Q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            frame_area,
        );
        assert!(!app.should_quit);

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(app.should_quit);
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        app.cancel_quit();
        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::ConfirmDelete(_))));
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt, None);
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            frame_area,
        );
        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt, None);
        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
    }

    #[test]
    fn ctrl_x_adds_a_claude_code_agent_chat() {
        let store = test_state_store("ctrl-x-adds-a-claude-code-agent-chat");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let workspace = app.project.workspaces[0].id;
        assert!(app.project.workspaces[0].chats.is_empty());

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            frame_area,
        );

        // Ctrl+x adds and selects a chat backed by Claude Code, distinct from
        // the pi chat that Ctrl+a creates.
        assert_eq!(app.project.workspaces[0].chats.len(), 1);
        assert_eq!(
            app.project.workspaces[0].chats[0].agent,
            AgentKind::ClaudeCode
        );
        let chat = app.project.workspaces[0].chats[0].id;
        assert_eq!(app.selected_item(), Some(NavItem::Chat { workspace, chat }));
    }

    #[test]
    fn ctrl_c_is_not_a_command_terminal_shortcut() {
        let store = test_state_store("ctrl-c-is-not-a-command-terminal-shortcu");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            frame_area,
        );

        assert_eq!(app.prompt, None);
        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
    }

    #[test]
    fn ctrl_shift_c_is_copy_shortcut_not_pty_interrupt() {
        let store = test_state_store("ctrl-shift-c-is-copy-shortcut-not-pty-in");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let key = KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );

        assert!(handle_control_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            key,
            frame_area,
        ));
        assert!(key_to_pty_bytes(key).is_empty());
        assert!(key_to_pty_bytes(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .is_empty());
    }

    #[test]
    fn ctrl_f_opens_workspace_prompt() {
        let store = test_state_store("ctrl-f-opens-workspace-prompt");
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        handle_unprompted_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt, Some(Prompt::OpenWorkspace(_))));
    }

    // ---- E4: the help overlay ---------------------------------------------

    #[test]
    fn f1_opens_help_over_a_selected_pty_but_a_bare_question_mark_does_not() {
        let store = test_state_store("help-f1");
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();

        // The seed state has a terminal selected, so `?` belongs to it.
        assert!(app.pty_input_target().is_some());
        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            frame_area,
        );
        assert!(!app.is_help_visible(), "? must reach a pane that wants it");

        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
            frame_area,
        );
        assert!(app.is_help_visible());
    }

    #[test]
    fn the_help_overlay_swallows_keys_and_closes_on_the_next_one() {
        let store = test_state_store("help-modal");
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        app.show_help();

        // A key aimed at the overlay must not start or type at a PTY behind it.
        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            frame_area,
        );
        assert!(!app.is_help_visible());
        assert!(!pty_runtime.is_running(app.pty_input_target().expect("a pane is selected")));

        // Quit still works from the overlay: it is checked before the overlay.
        app.show_help();
        handle_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_n_dismisses_notices_but_otherwise_reaches_the_pty() {
        let store = test_state_store("notice-dismiss");
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let mut app = App::default();
        let mut pty_runtime = PtyRuntime::new_offline();
        app.push_notice(
            NoticeLevel::Error,
            NoticeSource::Report,
            "daemon unreachable",
        );

        assert!(handle_control_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            frame_area,
        ));
        assert!(app.notices().is_empty());

        // With nothing to dismiss the key is not consumed, so a shell behind
        // the surface keeps its `Ctrl+n`.
        assert!(!handle_control_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            frame_area,
        ));
    }
}
