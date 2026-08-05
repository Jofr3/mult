//! One key handler per prompt, plus the editing skeleton they all share.
//!
//! Only `Enter` differs between the text prompts — it is the prompt's own
//! action — so each handler binds that and delegates the rest to
//! [`handle_common_prompt_key`]: cancel, list movement, kill-to-end, then text
//! editing. The four copies of that skeleton used to drift a key at a time
//! (F13).

use crate::{
    app::{App, StatusLevel},
    config::{Config, ConfiguredProject},
    layout::AppLayout,
    pty::PtyRuntime,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{
    input::{confirm_delete_selected, execute_command_action},
    keymap::is_unshifted_control_char,
    session::start_terminal,
};

/// The editing keys every prompt shares (E7): the cursor motions, the readline
/// deletions, and plain typing. Returns whether the key was consumed, so each
/// prompt handler keeps only the keys that are actually its own (submit,
/// cancel, list movement) and none of them owns a copy of this.
///
/// `Ctrl+k` is deliberately absent: it is the "select previous" half of the
/// Ctrl+j/Ctrl+k pair in the prompts that draw a result list, so the caller
/// decides — see [`handle_prompt_kill_to_end_key`].
fn handle_prompt_editing_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Backspace => app.pop_prompt_char(),
        KeyCode::Delete => app.delete_prompt_char_at_cursor(),
        KeyCode::Left => app.move_prompt_cursor_left(),
        KeyCode::Right => app.move_prompt_cursor_right(),
        KeyCode::Home => app.move_prompt_cursor_to_start(),
        KeyCode::End => app.move_prompt_cursor_to_end(),
        _ if is_unshifted_control_char(key, 'a') => app.move_prompt_cursor_to_start(),
        _ if is_unshifted_control_char(key, 'e') => app.move_prompt_cursor_to_end(),
        _ if is_unshifted_control_char(key, 'w') => app.delete_prompt_word_before_cursor(),
        _ if is_unshifted_control_char(key, 'u') => app.delete_prompt_to_start(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.push_prompt_char(c);
        }
        _ => return false,
    }
    true
}

/// `Ctrl+k` in a prompt with no result list under it: delete to end of line.
/// In the palette and the configured-project prompt the same key keeps its
/// existing meaning (select the previous match), which those handlers bind
/// before calling this.
fn handle_prompt_kill_to_end_key(app: &mut App, key: KeyEvent) -> bool {
    if !is_unshifted_control_char(key, 'k') || app.prompt_has_result_list() {
        return false;
    }
    app.delete_prompt_to_end();
    true
}

/// The result list under a prompt, if it has one. Decides whether the movement
/// keys select a match or fall through to text editing.
#[derive(Clone, Copy)]
enum PromptList<'a> {
    /// A plain text prompt: nothing to move through.
    None,
    OpenWorkspace(&'a [ConfiguredProject]),
    CommandPalette,
}

/// Everything the four text prompts answer the same way (F13).
///
/// Only `Enter` differs between them — it is the prompt's own action — so each
/// handler binds that and delegates the rest here: cancel, list movement,
/// kill-to-end, then text editing. The four copies of this skeleton used to
/// drift a key at a time.
///
/// Returns whether the key was consumed; an unconsumed key in a prompt is
/// ignored, never passed to a PTY.
fn handle_common_prompt_key(app: &mut App, key: KeyEvent, list: PromptList<'_>) -> bool {
    match key.code {
        KeyCode::Esc => app.cancel_prompt(),
        KeyCode::Up => return move_prompt_selection(app, list, true),
        KeyCode::Down => return move_prompt_selection(app, list, false),
        _ if is_unshifted_control_char(key, 'c') => app.cancel_prompt(),
        // Ctrl+k selects the previous match where a list is actually under the
        // prompt, and otherwise keeps its readline meaning of "kill to end of
        // line". Both halves of that resolution are here, so a new prompt
        // cannot pick one without the other.
        _ if is_unshifted_control_char(key, 'k') => {
            return move_prompt_selection(app, list, true)
                || handle_prompt_kill_to_end_key(app, key);
        }
        _ if is_unshifted_control_char(key, 'j') => return move_prompt_selection(app, list, false),
        _ => return handle_prompt_editing_key(app, key),
    }
    true
}

/// Move a prompt's result selection, reporting whether there was one to move.
fn move_prompt_selection(app: &mut App, list: PromptList<'_>, backwards: bool) -> bool {
    match list {
        PromptList::None => false,
        // The path form of the open-workspace prompt has no matches under it,
        // so its movement keys are not selection keys.
        PromptList::OpenWorkspace(_) if !app.prompt_has_result_list() => false,
        PromptList::OpenWorkspace(projects) => {
            if backwards {
                app.select_previous_open_workspace_match(projects);
            } else {
                app.select_next_open_workspace_match(projects);
            }
            true
        }
        PromptList::CommandPalette => {
            if backwards {
                app.select_previous_command_palette_entry();
            } else {
                app.select_next_command_palette_entry();
            }
            true
        }
    }
}

pub(super) fn handle_open_workspace_key(app: &mut App, config: &Config, key: KeyEvent) {
    if key.code == KeyCode::Enter {
        app.submit_open_workspace(&config.projects);
        return;
    }
    handle_common_prompt_key(app, key, PromptList::OpenWorkspace(&config.projects));
}

pub(super) fn handle_terminal_command_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Enter {
        app.submit_new_terminal_command();
        return;
    }
    handle_common_prompt_key(app, key, PromptList::None);
}

/// The confirmation in front of a destructive delete (E3). Enter and `y`
/// confirm; Esc, `n` and Ctrl+c cancel; anything else is ignored, so no stray
/// key deletes anything.
pub(super) fn handle_confirm_delete_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Enter => confirm_delete_selected(app, pty_runtime),
        KeyCode::Char('y') | KeyCode::Char('Y')
            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            confirm_delete_selected(app, pty_runtime);
        }
        KeyCode::Esc => app.cancel_prompt(),
        KeyCode::Char('n') | KeyCode::Char('N')
            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.cancel_prompt();
        }
        _ if is_unshifted_control_char(key, 'c') => app.cancel_prompt(),
        _ => {}
    }
}

/// The confirmation in front of replaying persisted command terminals (C1).
///
/// Same keys as the delete confirmation, and the same rule: only an explicit
/// yes runs anything, and every other key is ignored rather than guessed at.
/// Declining is not silent — the terminals stay in the sidebar, stopped, and the
/// status line says why.
pub(super) fn handle_confirm_restore_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    layout: &AppLayout,
) {
    let confirm = match key.code {
        KeyCode::Enter => true,
        KeyCode::Char('y') | KeyCode::Char('Y')
            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            true
        }
        KeyCode::Esc => false,
        KeyCode::Char('n') | KeyCode::Char('N')
            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            false
        }
        _ if is_unshifted_control_char(key, 'c') => false,
        _ => return,
    };

    if !confirm {
        let declined = app.decline_restore();
        if declined > 0 {
            let plural = if declined == 1 { "" } else { "s" };
            app.push_status_notice(
                StatusLevel::Info,
                format!("{declined} saved command terminal{plural} left stopped"),
            );
        }
        return;
    }

    for entry in app.confirm_restore() {
        start_terminal(
            app,
            pty_runtime,
            config,
            layout,
            entry.workspace,
            entry.terminal,
        );
    }
}

/// The keybinding overlay (E4): scrollable, and closed by anything that reads
/// as "done" — including `?`/`F1` again.
pub(super) fn handle_help_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up => app.scroll_help(-1),
        KeyCode::Down => app.scroll_help(1),
        KeyCode::PageUp => app.scroll_help(-10),
        KeyCode::PageDown => app.scroll_help(10),
        KeyCode::Home => app.scroll_help(isize::MIN),
        KeyCode::End => app.scroll_help(isize::MAX),
        _ if is_unshifted_control_char(key, 'k') => app.scroll_help(-1),
        _ if is_unshifted_control_char(key, 'j') => app.scroll_help(1),
        _ => app.cancel_prompt(),
    }
}

pub(super) fn handle_command_palette_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    key: KeyEvent,
    layout: &AppLayout,
) {
    if key.code == KeyCode::Enter {
        if let Some(action) = app.submit_command_palette() {
            execute_command_action(app, pty_runtime, config, action, layout);
        }
        return;
    }
    handle_common_prompt_key(app, key, PromptList::CommandPalette);
}

pub(super) fn handle_search_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Enter {
        app.submit_search();
        return;
    }
    handle_common_prompt_key(app, key, PromptList::None);
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    /// The handlers take the frame's resolved [`AppLayout`]; these tests
    /// describe a frame *size*, so each wrapper resolves the layout from the
    /// app's current state first — exactly what the loop does before it draws.
    fn confirm_restore(
        app: &mut App,
        pty_runtime: &mut PtyRuntime,
        config: &Config,
        key: KeyEvent,
        frame_area: Rect,
    ) {
        let layout = AppLayout::compute(app, frame_area);
        handle_confirm_restore_key(app, pty_runtime, config, key, &layout);
    }

    fn palette_key(
        app: &mut App,
        pty_runtime: &mut PtyRuntime,
        config: &Config,
        key: KeyEvent,
        frame_area: Rect,
    ) {
        let layout = AppLayout::compute(app, frame_area);
        handle_command_palette_key(app, pty_runtime, config, key, &layout);
    }
    use super::*;
    use crate::{
        app::{PendingRestore, Prompt},
        model::{self, PtyKey},
    };

    #[test]
    fn declining_the_restore_prompt_starts_nothing_and_reports_it() {
        let mut app = App::two_workspaces();
        let workspace = app.project.workspaces[0].id;
        let terminal = app
            .project
            .add_command_terminal(
                workspace,
                "planted".to_string(),
                true,
                "rm -rf ~".to_string(),
            )
            .expect("add command terminal");
        app.request_restore_confirmation(vec![PendingRestore {
            workspace,
            terminal,
            name: "planted".to_string(),
            command: "rm -rf ~".to_string(),
        }]);
        let mut pty_runtime = PtyRuntime::new_offline();

        confirm_restore(
            &mut app,
            &mut pty_runtime,
            &Config::default(),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            Rect::new(0, 0, 80, 24),
        );

        assert!(app.prompt().is_none());
        assert!(!pty_runtime.is_running(PtyKey::Terminal(terminal)));
        let notice = app
            .current_status_notice()
            .expect("declining is reported, not silent");
        assert!(
            notice.message.contains("left stopped"),
            "{}",
            notice.message
        );
    }
    /// C1: a stray key at the prompt neither runs nor dismisses anything.

    #[test]
    fn an_unrelated_key_leaves_the_restore_prompt_open() {
        let mut app = App::two_workspaces();
        let workspace = app.project.workspaces[0].id;
        app.request_restore_confirmation(vec![PendingRestore {
            workspace,
            terminal: model::TerminalId::new(4_242).unwrap(),
            name: "planted".to_string(),
            command: "echo hi".to_string(),
        }]);
        let mut pty_runtime = PtyRuntime::new_offline();

        confirm_restore(
            &mut app,
            &mut pty_runtime,
            &Config::default(),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            Rect::new(0, 0, 80, 24),
        );

        assert!(matches!(app.prompt(), Some(Prompt::ConfirmRestore(_))));
    }

    #[test]
    fn prompt_keys_move_the_cursor_and_delete_around_it() {
        let mut app = App::two_workspaces();
        let config = Config::default();
        app.begin_new_terminal_command();
        for ch in "cargo test".chars() {
            app.push_prompt_char(ch);
        }

        let press = |app: &mut App, code: KeyCode, modifiers: KeyModifiers| {
            handle_terminal_command_key(app, KeyEvent::new(code, modifiers));
        };
        let input = |app: &App| match app.prompt() {
            Some(Prompt::NewTerminalCommand(prompt)) => prompt.input.as_str().to_string(),
            other => panic!("expected the command prompt, got {other:?}"),
        };

        press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(input(&app), "cargo ");

        press(&mut app, KeyCode::Home, KeyModifiers::NONE);
        press(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(input(&app), "argo ");

        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(input(&app), "a");

        press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(input(&app), "na");

        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(input(&app), "");

        // In the palette the same Ctrl+k keeps its list meaning instead.
        app.cancel_prompt();
        app.begin_command_palette();
        for ch in "focus".chars() {
            app.push_prompt_char(ch);
        }
        let mut pty_runtime = PtyRuntime::new_offline();
        palette_key(
            &mut app,
            &mut pty_runtime,
            &config,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            Rect::new(0, 0, 120, 40),
        );
        let Some(Prompt::CommandPalette(prompt)) = app.prompt() else {
            panic!("expected the palette to stay open");
        };
        assert_eq!(prompt.input.as_str(), "focus");
        assert!(prompt.selected.index() > 0);
    }

    #[test]
    fn open_workspace_prompt_ctrl_j_and_ctrl_k_select_matches() {
        let mut app = App::two_workspaces();
        let config = Config {
            projects: vec![
                crate::config::ConfiguredProject {
                    name: "first".to_string(),
                    path: "/tmp/first".into(),
                },
                crate::config::ConfiguredProject {
                    name: "second".to_string(),
                    path: "/tmp/second".into(),
                },
            ],
            ..Config::default()
        };

        app.begin_open_workspace(&config.projects);
        handle_open_workspace_key(
            &mut app,
            &config,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            app.prompt(),
            Some(Prompt::OpenWorkspace(ref prompt)) if prompt.selected.index() == 1
        ));

        handle_open_workspace_key(
            &mut app,
            &config,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert!(matches!(
            app.prompt(),
            Some(Prompt::OpenWorkspace(ref prompt)) if prompt.selected.index() == 0
        ));
    }
}
