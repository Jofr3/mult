//! Keyboard and paste handling: the event dispatch, the unprompted key
//! bindings, and one handler per prompt.
//!
//! The prompts share [`handle_common_prompt_key`] so that only `Enter` — the
//! prompt's own action — is written per prompt; the four copies of the editing
//! skeleton used to drift a key at a time (F13).

use crate::{
    app::{App, CommandAction, DeleteRequest, NavItem, Prompt},
    config::Config,
    layout::AppLayout,
    model::{AgentKind, PtyKey},
    pty::PtyRuntime,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{
    clipboard::copy_current_text_selection,
    keymap::{
        is_control_down_key, is_control_key, is_control_up_key, is_quit_key,
        is_shifted_control_char, is_unshifted_control_char, key_to_pty_bytes_in_mode,
    },
    mouse::handle_mouse,
    prompts::{
        handle_command_palette_key, handle_confirm_delete_key, handle_confirm_restore_key,
        handle_help_key, handle_open_workspace_key, handle_search_key, handle_terminal_command_key,
    },
    session::{
        start_or_focus_chat_agent, start_or_focus_selected_chat_agent,
        start_or_focus_selected_terminal, start_terminal,
    },
};

pub(super) fn handle_event(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    event: Event,
    layout: &AppLayout,
) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, pty_runtime, config, key, layout);
        }
        Event::Mouse(mouse) => handle_mouse(app, pty_runtime, config, mouse, layout),
        Event::Paste(text) => handle_paste(app, pty_runtime, config, text, layout),
        _ => {}
    }
}

fn handle_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    layout: &AppLayout,
) {
    if is_quit_key(key) {
        app.quit();
        return;
    }

    match app.prompt() {
        Some(Prompt::OpenWorkspace(_)) => handle_open_workspace_key(app, config, key),
        Some(Prompt::NewTerminalCommand(_)) => handle_terminal_command_key(app, key),
        Some(Prompt::CommandPalette(_)) => {
            handle_command_palette_key(app, pty_runtime, config, key, layout);
        }
        Some(Prompt::Search(_)) => handle_search_key(app, key),
        Some(Prompt::ConfirmDelete(_)) => handle_confirm_delete_key(app, pty_runtime, key),
        Some(Prompt::ConfirmRestore(_)) => {
            handle_confirm_restore_key(app, pty_runtime, config, key, layout);
        }
        Some(Prompt::Help(_)) => handle_help_key(app, key),
        None => handle_unprompted_key(app, pty_runtime, config, key, layout),
    }
}

fn handle_paste(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    text: String,
    layout: &AppLayout,
) {
    if app.is_prompt_active() {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            app.push_prompt_char(ch);
        }
        return;
    }

    let Some(terminal_id) = start_selected_pty_if_needed(app, pty_runtime, config, layout) else {
        return;
    };

    match pty_runtime.send_paste(terminal_id, &text) {
        Ok(true) => {}
        Ok(false) => {
            pty_runtime.append_pty_system_line(terminal_id, "PTY is not running");
        }
        Err(error) => {
            pty_runtime.append_pty_system_line(terminal_id, format!("failed to paste: {error}"));
        }
    }
}

fn handle_unprompted_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    layout: &AppLayout,
) {
    if handle_control_key(app, pty_runtime, config, key, layout) {
        return;
    }

    handle_selected_pty_input_key(app, pty_runtime, config, key, layout);
}

fn handle_control_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    layout: &AppLayout,
) -> bool {
    if is_shifted_control_char(key, 'c') {
        let _ = copy_current_text_selection(app, pty_runtime, config);
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
        request_delete_selected(app, pty_runtime);
        return true;
    }
    if is_help_key(app, key) {
        app.show_help();
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
        add_agent_to_selected_workspace(app, pty_runtime, config, layout, AgentKind::Pi);
        return true;
    }
    if is_unshifted_control_char(key, 'x') {
        add_agent_to_selected_workspace(app, pty_runtime, config, layout, AgentKind::ClaudeCode);
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
    // Only swallowed when there is actually something to dismiss, so Ctrl+g
    // still reaches the focused PTY the rest of the time.
    if is_unshifted_control_char(key, 'g') && app.dismiss_status_notice() {
        return true;
    }

    false
}

/// Whether `key` opens the keybinding overlay (E4).
///
/// `F1` always does: it is the one key `mult` keeps for itself, so help is
/// reachable even while a full-screen program owns the pane. A bare `?` opens
/// help only when no PTY could have received it — otherwise typing `?` into a
/// shell would open a help overlay instead of reaching the shell, and every
/// plain key is PTY input the moment a chat or terminal is selected.
fn is_help_key(app: &App, key: KeyEvent) -> bool {
    if matches!(key.code, KeyCode::F(1)) && !is_control_key(key) {
        return true;
    }

    matches!(key.code, KeyCode::Char('?'))
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && app.pty_input_target().is_none()
}

fn add_agent_to_selected_workspace(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
    agent: AgentKind,
) {
    if let Some((workspace, chat)) = app.add_chat_to_selected_workspace_and_return(agent) {
        start_or_focus_chat_agent(app, pty_runtime, config, layout, workspace, chat, true);
    }
}

/// Ask to delete the selection (E3). Only a provably empty item goes without a
/// confirmation prompt, and whether it is empty depends on runtime state the
/// `App` cannot see, so that is answered here.
fn request_delete_selected(app: &mut App, pty_runtime: &mut PtyRuntime) {
    let pty_has_content = app
        .pty_input_target()
        .is_some_and(|terminal| pty_holds_content(pty_runtime, terminal));
    if let DeleteRequest::Deleted(terminals) = app.request_delete_selected(pty_has_content) {
        stop_deleted_ptys(pty_runtime, terminals);
    }
}

pub(super) fn confirm_delete_selected(app: &mut App, pty_runtime: &mut PtyRuntime) {
    let terminals = app.confirm_delete();
    stop_deleted_ptys(pty_runtime, terminals);
}

fn stop_deleted_ptys(pty_runtime: &mut PtyRuntime, terminals: Vec<PtyKey>) {
    for terminal in terminals {
        let _ = pty_runtime.stop(terminal);
        pty_runtime.remove_pty(terminal);
    }
}

/// Whether deleting this PTY would throw anything away: a live process, or a
/// screen that still has output on it.
fn pty_holds_content(pty_runtime: &PtyRuntime, terminal: PtyKey) -> bool {
    pty_runtime.is_running(terminal) || !pty_runtime.pty_output_is_blank(terminal)
}

fn handle_selected_pty_input_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    layout: &AppLayout,
) {
    // Emptiness does not depend on cursor-key mode, so this also avoids starting
    // a PTY for keys that map to nothing (e.g. shortcuts handled elsewhere).
    if key_to_pty_bytes_in_mode(key, false).is_empty() {
        return;
    }

    let Some(terminal_id) = start_selected_pty_if_needed(app, pty_runtime, config, layout) else {
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
            pty_runtime.append_pty_system_line(terminal_id, "PTY is not running");
        }
        Err(error) => {
            pty_runtime
                .append_pty_system_line(terminal_id, format!("failed to send input: {error}"));
        }
    }
}

fn start_selected_pty_if_needed(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
) -> Option<PtyKey> {
    match app.selected_item()? {
        NavItem::Chat { workspace, chat } => {
            let terminal = PtyKey::ChatAgent(chat);
            if pty_runtime.is_running(terminal) {
                app.begin_chat_agent_input();
            } else {
                start_or_focus_chat_agent(app, pty_runtime, config, layout, workspace, chat, true);
            }
            pty_runtime.is_running(terminal).then_some(terminal)
        }
        NavItem::Terminal {
            workspace,
            terminal,
        } => {
            let key = PtyKey::Terminal(terminal);
            if !pty_runtime.is_running(key) {
                start_terminal(app, pty_runtime, config, layout, workspace, terminal);
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

pub(super) fn execute_command_action(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    action: CommandAction,
    layout: &AppLayout,
) {
    match action {
        CommandAction::FocusSidebar => app.focus_sidebar(),
        CommandAction::FocusSelectedPane => {
            app.focus_selected_main();
        }
        CommandAction::StartInput => focus_selected_input(app, pty_runtime, config, layout),
        CommandAction::AddAgentChat => {
            add_agent_to_selected_workspace(app, pty_runtime, config, layout, AgentKind::Pi);
        }
        CommandAction::AddClaudeCodeChat => {
            add_agent_to_selected_workspace(
                app,
                pty_runtime,
                config,
                layout,
                AgentKind::ClaudeCode,
            );
        }
        CommandAction::AddShellTerminal => app.add_terminal_to_selected_workspace(),
        CommandAction::AddCommandTerminal => {
            app.begin_new_terminal_command();
        }
        CommandAction::OpenWorkspace => app.begin_open_workspace(&config.projects),
        CommandAction::DeleteSelected => request_delete_selected(app, pty_runtime),
        CommandAction::SearchSelectedPane => {
            app.begin_search();
        }
        CommandAction::ClearSearch => app.clear_search(),
        // The loop owns the `Config`, so the palette only records the request
        // and the swap happens there (E9).
        CommandAction::ReloadConfig => app.request_config_reload(),
        CommandAction::DismissStatusNotice => {
            app.dismiss_status_notice();
        }
        CommandAction::ShowHelp => app.show_help(),
        CommandAction::Quit => app.quit(),
    }
}

fn focus_selected_input(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    layout: &AppLayout,
) {
    if app.selected_chat_id().is_some() {
        start_or_focus_selected_chat_agent(app, pty_runtime, config, layout);
    } else if app.selected_terminal_id().is_some() {
        start_or_focus_selected_terminal(app, pty_runtime, config, layout);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    /// The handlers take the frame's resolved [`AppLayout`]; these tests
    /// describe a frame *size*, so each wrapper resolves the layout from the
    /// app's current state first — exactly what the loop does before it draws.
    fn unprompted(
        app: &mut App,
        pty_runtime: &mut PtyRuntime,
        config: &Config,
        key: KeyEvent,
        frame_area: Rect,
    ) {
        let layout = AppLayout::compute(app, frame_area);
        handle_unprompted_key(app, pty_runtime, config, key, &layout);
    }

    fn keyed(
        app: &mut App,
        pty_runtime: &mut PtyRuntime,
        config: &Config,
        key: KeyEvent,
        frame_area: Rect,
    ) {
        let layout = AppLayout::compute(app, frame_area);
        handle_key(app, pty_runtime, config, key, &layout);
    }

    fn control(
        app: &mut App,
        pty_runtime: &mut PtyRuntime,
        config: &Config,
        key: KeyEvent,
        frame_area: Rect,
    ) -> bool {
        let layout = AppLayout::compute(app, frame_area);
        handle_control_key(app, pty_runtime, config, key, &layout)
    }
    use super::{super::keymap::key_to_pty_bytes, *};

    /// C1: declining leaves everything stopped and says so, rather than looking
    /// like the terminals silently failed.
    #[test]
    fn ctrl_g_dismisses_a_status_notice_and_is_otherwise_left_to_the_pty() {
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        app.set_last_error("failed to save state: No space left on device");

        let key = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(control(
            &mut app,
            &mut pty_runtime,
            &config,
            key,
            frame_area
        ));
        assert!(app.current_status_notice().is_none());
        // With nothing to dismiss the key is not swallowed, so a PTY that binds
        // Ctrl+g still receives it.
        assert!(!control(
            &mut app,
            &mut pty_runtime,
            &config,
            key,
            frame_area
        ));
    }
    #[test]
    fn ctrl_j_and_ctrl_k_navigate_selection() {
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(app.selected_index(), Some(1));
        assert_eq!(app.selected_item(), Some(app.nav_items()[1]));

        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(app.selected_index(), Some(0));
        assert_eq!(app.selected_item(), Some(app.nav_items()[0]));
    }
    #[test]
    fn ctrl_p_opens_palette_and_ctrl_s_opens_search_for_selected_pane() {
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt(), Some(Prompt::CommandPalette(_))));
        app.cancel_prompt();

        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        let target = NavItem::Terminal {
            workspace,
            terminal,
        };
        app.select_item(target);
        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt(), Some(Prompt::Search(_))));
    }
    #[test]
    fn plain_keys_are_not_workspace_commands() {
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            frame_area,
        );
        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            frame_area,
        );

        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
        assert!(!app.should_quit);
        assert_eq!(app.prompt(), None);
    }
    #[test]
    fn f1_opens_help_and_a_question_mark_meant_for_a_pty_is_left_alone() {
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        // A terminal is selected, so `?` is PTY input and must stay that way.
        assert!(app.pty_input_target().is_some());
        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt(), None);

        // F1 is the one key mult keeps for itself.
        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
            frame_area,
        );
        assert!(matches!(app.prompt(), Some(Prompt::Help(_))));

        // Any other key closes it again, and does not reach the PTY behind it.
        keyed(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt(), None);

        // With nothing selected there is no PTY to steal from, so `?` works.
        app.project.workspaces.clear();
        app.select_nav_index(0);
        assert_eq!(app.pty_input_target(), None);
        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            frame_area,
        );
        assert!(matches!(app.prompt(), Some(Prompt::Help(_))));
    }
    #[test]
    fn a_confirmation_only_deletes_on_yes_and_stops_the_pty_when_it_does() {
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        // Output on the screen is content the delete would throw away.
        pty_runtime.append_pty_system_line(PtyKey::Terminal(terminal), "hello");

        request_delete_selected(&mut app, &mut pty_runtime);
        assert!(matches!(app.prompt(), Some(Prompt::ConfirmDelete(_))));

        // A key that is neither yes nor no leaves the prompt (and the item) up.
        keyed(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            frame_area,
        );
        assert!(matches!(app.prompt(), Some(Prompt::ConfirmDelete(_))));

        keyed(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt(), None);
        assert!(app.project.terminal(workspace, terminal).is_some());
        assert!(!pty_runtime.pty_output_is_blank(PtyKey::Terminal(terminal)));

        request_delete_selected(&mut app, &mut pty_runtime);
        keyed(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt(), None);
        assert!(app.project.terminal(workspace, terminal).is_none());
        // Confirming also drops the runtime pane, not just the durable entry.
        assert!(pty_runtime.pty_output_is_blank(PtyKey::Terminal(terminal)));
    }
    #[test]
    fn ctrl_keys_create_delete_and_quit() {
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        keyed(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(
                KeyCode::Char('Q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            frame_area,
        );
        assert!(!app.should_quit);

        keyed(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(app.should_quit);
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        app.should_quit = false;
        // Ctrl+Q above started the selected terminal's PTY, so it now has
        // output on it: Ctrl+q asks instead of deleting (E3).
        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt(), Some(Prompt::ConfirmDelete(_))));
        assert_eq!(
            app.project.workspaces[0].terminals.len(),
            initial_terminals + 1
        );

        keyed(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            frame_area,
        );
        assert_eq!(app.prompt(), None);
        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
    }
    #[test]
    fn ctrl_x_adds_a_claude_code_agent_chat() {
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let workspace = app.project.workspaces[0].id;
        assert!(app.project.workspaces[0].chats.is_empty());

        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
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
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let initial_terminals = app.project.workspaces[0].terminals.len();

        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            frame_area,
        );

        assert_eq!(app.prompt(), None);
        assert_eq!(app.project.workspaces[0].terminals.len(), initial_terminals);
    }
    #[test]
    fn ctrl_shift_c_is_copy_shortcut_not_pty_interrupt() {
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);
        let key = KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );

        assert!(control(
            &mut app,
            &mut pty_runtime,
            &config,
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
        let mut app = App::two_workspaces();
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let frame_area = Rect::new(0, 0, 120, 40);

        unprompted(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            frame_area,
        );
        assert!(matches!(app.prompt(), Some(Prompt::OpenWorkspace(_))));
    }
}
