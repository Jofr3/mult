//! The prompt state machines: one shared key classifier plus the per-prompt
//! handlers, and the command-palette actions they can submit.

use std::fs::{self};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::layout::AppLayout;
use crate::{
    app::{App, CommandAction, PromptEdit},
    config::Config,
    model::{AgentKind, PtyKey},
    pty::PtyRuntime,
    storage,
};

use super::agent_launch::add_agent_to_selected_workspace;
use super::agent_status::mult_agent_status_path;
use super::input::focus_selected_input;
use super::keymap::is_unshifted_control_char;

fn confirm_pending_delete(app: &mut App, pty_runtime: &mut PtyRuntime) {
    let status_file = app.pending_delete_pty().and_then(|key| match key {
        PtyKey::ChatAgent(chat) => Some(mult_agent_status_path(
            app.project.session_identity(key)?,
            app.project.active_agent_generation(chat)?,
        )),
        PtyKey::Terminal(_) => None,
    });
    let removed = confirm_pending_delete_with(app, |_, terminal| {
        pty_runtime
            .stop(terminal)
            .map(|_| ())
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
    });
    for terminal in &removed {
        pty_runtime.remove_terminal(*terminal);
    }
    if !removed.is_empty() {
        if let Some(path) = status_file {
            let _ = fs::remove_file(path);
        }
    }
}

fn confirm_pending_delete_with(
    app: &mut App,
    mut stop: impl FnMut(&App, PtyKey) -> Result<(), Box<dyn std::error::Error>>,
) -> Vec<PtyKey> {
    if let Some(terminal) = app.pending_delete_pty() {
        // The target is still present here. Durable state is mutated only after
        // the daemon accepted the stop request (or no attachment existed).
        if let Err(error) = stop(app, terminal) {
            app.set_delete_error(format!("failed to stop PTY; item was not deleted: {error}"));
            return Vec::new();
        }
    }

    app.confirm_delete()
}

/// What a key means inside a prompt, independent of which prompt it is.
///
/// The four prompt handlers used to repeat the same Esc/Enter/Backspace/Char
/// skeleton with slightly different holes in it (F13). They now share this
/// classifier and differ only in what "submit" and "move" mean for them, so a
/// prompt cannot silently be missing an editing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKey {
    Cancel,
    Submit,
    /// Move the list selection, where the prompt has a list.
    Previous,
    Next,
    Edit(PromptEdit),
    Ignored,
}

/// `Ctrl+k` is deliberately **not** readline's kill-to-end-of-line here.
///
/// It is `mult`'s global "previous item" (the pair of `Ctrl+j`), and two of the
/// five prompts show a list it moves through. Rebinding it to a kill inside the
/// other prompts would make one key mean two things depending on which prompt
/// happened to be open, which is worse than lacking a kill: `Ctrl+u` (delete to
/// start) and `Ctrl+w` (delete the previous word) cover the same ground, and
/// `Delete` removes the character after the cursor. So `Ctrl+k` means
/// "previous" in every prompt, and does nothing in the prompts with no list.
fn classify_prompt_key(key: KeyEvent) -> PromptKey {
    match key.code {
        KeyCode::Esc => PromptKey::Cancel,
        KeyCode::Enter => PromptKey::Submit,
        KeyCode::Up => PromptKey::Previous,
        KeyCode::Down => PromptKey::Next,
        KeyCode::Left => PromptKey::Edit(PromptEdit::MoveLeft),
        KeyCode::Right => PromptKey::Edit(PromptEdit::MoveRight),
        KeyCode::Home => PromptKey::Edit(PromptEdit::MoveHome),
        KeyCode::End => PromptKey::Edit(PromptEdit::MoveEnd),
        KeyCode::Backspace => PromptKey::Edit(PromptEdit::Backspace),
        KeyCode::Delete => PromptKey::Edit(PromptEdit::DeleteForward),
        _ if is_unshifted_control_char(key, 'c') => PromptKey::Cancel,
        _ if is_unshifted_control_char(key, 'k') => PromptKey::Previous,
        _ if is_unshifted_control_char(key, 'j') => PromptKey::Next,
        _ if is_unshifted_control_char(key, 'a') => PromptKey::Edit(PromptEdit::MoveHome),
        _ if is_unshifted_control_char(key, 'e') => PromptKey::Edit(PromptEdit::MoveEnd),
        _ if is_unshifted_control_char(key, 'w') => PromptKey::Edit(PromptEdit::DeleteWordBefore),
        _ if is_unshifted_control_char(key, 'u') => PromptKey::Edit(PromptEdit::DeleteToStart),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            PromptKey::Edit(PromptEdit::Insert(c))
        }
        _ => PromptKey::Ignored,
    }
}

/// Handle everything about a prompt key that does not depend on the prompt, and
/// report what is left for the caller.
fn apply_shared_prompt_key(app: &mut App, key: KeyEvent) -> PromptKey {
    let classified = classify_prompt_key(key);
    match classified {
        PromptKey::Cancel => app.cancel_prompt(),
        PromptKey::Edit(edit) => {
            app.apply_prompt_edit(edit);
        }
        PromptKey::Submit | PromptKey::Previous | PromptKey::Next | PromptKey::Ignored => {}
    }
    classified
}

pub(super) fn handle_open_workspace_key(app: &mut App, config: &Config, key: KeyEvent) {
    match apply_shared_prompt_key(app, key) {
        PromptKey::Submit => app.submit_open_workspace(&config.projects),
        PromptKey::Previous => app.select_previous_open_workspace_match(&config.projects),
        PromptKey::Next => app.select_next_open_workspace_match(&config.projects),
        PromptKey::Cancel | PromptKey::Edit(_) | PromptKey::Ignored => {}
    }
}

pub(super) fn handle_terminal_command_key(app: &mut App, key: KeyEvent) {
    if apply_shared_prompt_key(app, key) == PromptKey::Submit {
        app.submit_new_terminal_command();
    }
}

pub(super) fn handle_command_palette_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    key: KeyEvent,
    layout: AppLayout,
) {
    match apply_shared_prompt_key(app, key) {
        PromptKey::Submit => {
            if let Some(action) = app.submit_command_palette() {
                execute_command_action(app, pty_runtime, config, store, action, layout);
            }
        }
        PromptKey::Previous => app.select_previous_command_palette_entry(),
        PromptKey::Next => app.select_next_command_palette_entry(),
        PromptKey::Cancel | PromptKey::Edit(_) | PromptKey::Ignored => {}
    }
}

pub(super) fn handle_search_key(app: &mut App, key: KeyEvent) {
    if apply_shared_prompt_key(app, key) == PromptKey::Submit {
        app.submit_search();
    }
}

pub(super) fn handle_delete_confirmation_key(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    key: KeyEvent,
) {
    match key.code {
        KeyCode::Esc => app.cancel_prompt(),
        KeyCode::Enter => confirm_pending_delete(app, pty_runtime),
        _ if is_unshifted_control_char(key, 'c') => app.cancel_prompt(),
        _ => {}
    }
}

fn execute_command_action(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    action: CommandAction,
    layout: AppLayout,
) {
    match action {
        CommandAction::ShowKeybindings => app.show_help(),
        CommandAction::DismissNotices => {
            app.dismiss_notices();
        }
        CommandAction::ReloadConfig => app.request_config_reload(),
        CommandAction::FocusSidebar => app.focus_sidebar(),
        CommandAction::FocusSelectedPane => {
            app.focus_selected_main();
        }
        CommandAction::StartInput => focus_selected_input(app, pty_runtime, config, store, layout),
        CommandAction::AddAgentChat => {
            add_agent_to_selected_workspace(app, pty_runtime, config, store, layout, AgentKind::Pi);
        }
        CommandAction::AddClaudeCodeChat => {
            add_agent_to_selected_workspace(
                app,
                pty_runtime,
                config,
                store,
                layout,
                AgentKind::ClaudeCode,
            );
        }
        CommandAction::AddShellTerminal => app.add_terminal_to_selected_workspace(),
        CommandAction::AddCommandTerminal => {
            app.begin_new_terminal_command();
        }
        CommandAction::OpenWorkspace => app.begin_open_workspace(&config.projects),
        CommandAction::DeleteSelected => {
            app.begin_delete_selected();
        }
        CommandAction::SearchSelectedPane => {
            app.begin_search();
        }
        CommandAction::ClearSearch => app.clear_search(),
        CommandAction::Quit => app.quit(),
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::app::NavItem;
    use crate::app::Prompt;
    use crate::runtime::test_support::*;
    use ratatui::layout::Rect;
    use std::io;

    #[test]
    fn delete_stop_failure_keeps_the_target_and_confirmation_open() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        app.mark_clean();
        assert!(app.begin_delete_selected());

        let removed = confirm_pending_delete_with(&mut app, |current, key| {
            assert_eq!(key, PtyKey::Terminal(terminal));
            assert!(current.project.terminal(workspace, terminal).is_some());
            Err(Box::new(io::Error::other("daemon refused stop")))
        });

        assert!(removed.is_empty());
        assert!(app.project.terminal(workspace, terminal).is_some());
        assert!(!app.is_dirty());
        assert!(matches!(
            app.prompt(),
            Some(Prompt::ConfirmDelete(ref prompt))
                if prompt.error.as_deref().is_some_and(|error| error.contains("daemon refused stop"))
        ));
    }

    #[test]
    fn successful_delete_stops_before_mutating_project_state() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        assert!(app.begin_delete_selected());

        let removed = confirm_pending_delete_with(&mut app, |current, key| {
            assert_eq!(key, PtyKey::Terminal(terminal));
            assert!(current.project.terminal(workspace, terminal).is_some());
            Ok(())
        });

        assert_eq!(removed, vec![PtyKey::Terminal(terminal)]);
        assert!(app.project.terminal(workspace, terminal).is_none());
    }

    #[test]
    fn open_workspace_prompt_ctrl_j_and_ctrl_k_select_matches() {
        let mut app = App::default();
        let config = config_with(|config| {
            config.projects = vec![
                crate::config::ConfiguredProject {
                    name: "first".to_string(),
                    path: "/tmp/first".into(),
                },
                crate::config::ConfiguredProject {
                    name: "second".to_string(),
                    path: "/tmp/second".into(),
                },
            ];
        });

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

    // ---- E7 / F13: one prompt-key path ---------------------------------

    /// One prompt's key handler, boxed so several of them can be driven by a
    /// single loop. Named because the tuple it sits in is otherwise too dense
    /// to read (and `clippy::type_complexity` says so).
    type PromptDrive = Box<dyn FnMut(&mut App, KeyEvent)>;

    /// Every text prompt shares one classifier, so the motions and kills are
    /// present in all of them rather than in whichever one was edited last.
    #[test]
    fn prompt_motions_and_kills_work_in_every_text_prompt() {
        let config = Config::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let ctrl = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);

        // Two prompts with a list and two without, driven through their own
        // handlers exactly as `handle_key` dispatches them.
        let mut drives: Vec<(&str, PromptDrive)> = vec![
            (
                "open workspace",
                Box::new(move |app: &mut App, event| {
                    handle_open_workspace_key(app, &Config::default(), event)
                }),
            ),
            (
                "new terminal command",
                Box::new(|app: &mut App, event| handle_terminal_command_key(app, event)),
            ),
            (
                "search",
                Box::new(|app: &mut App, event| handle_search_key(app, event)),
            ),
        ];

        for (name, drive) in &mut drives {
            let mut app = App::default();
            match *name {
                "open workspace" => app.begin_open_workspace(&config.projects),
                "new terminal command" => {
                    assert!(app.begin_new_terminal_command(), "{name}");
                }
                _ => assert!(app.begin_search(), "{name}"),
            }
            // Start from a known state: the Open-Workspace prompt pre-fills the
            // working directory.
            while app.prompt_input().is_some_and(|input| !input.is_empty()) {
                drive(&mut app, ctrl('u'));
                drive(&mut app, key(KeyCode::Delete));
            }

            for ch in "cargo test".chars() {
                drive(&mut app, key(KeyCode::Char(ch)));
            }
            assert_eq!(app.prompt_input().unwrap().as_str(), "cargo test", "{name}");

            drive(&mut app, key(KeyCode::Home));
            assert_eq!(app.prompt_input().unwrap().cursor(), 0, "{name}");
            drive(&mut app, key(KeyCode::Delete));
            assert_eq!(app.prompt_input().unwrap().as_str(), "argo test", "{name}");
            drive(&mut app, key(KeyCode::End));
            drive(&mut app, ctrl('w'));
            assert_eq!(app.prompt_input().unwrap().as_str(), "argo ", "{name}");
            drive(&mut app, key(KeyCode::Left));
            drive(&mut app, key(KeyCode::Char('X')));
            assert_eq!(app.prompt_input().unwrap().as_str(), "argoX ", "{name}");
            drive(&mut app, ctrl('a'));
            assert_eq!(app.prompt_input().unwrap().cursor(), 0, "{name}");
            drive(&mut app, ctrl('e'));
            assert_eq!(app.prompt_input().unwrap().cursor(), 6, "{name}");
            drive(&mut app, ctrl('u'));
            assert_eq!(app.prompt_input().unwrap().as_str(), "", "{name}");

            // ...and cancelling is the same key everywhere.
            drive(&mut app, ctrl('c'));
            assert!(app.prompt().is_none(), "{name}");
        }
    }

    /// `Ctrl+k` keeps meaning "previous", not readline's kill-to-end-of-line.
    /// See `classify_prompt_key` for why: one key, one meaning, across every
    /// prompt — `Ctrl+u`/`Ctrl+w`/`Delete` are the deletions.
    #[test]
    fn ctrl_k_moves_the_selection_and_never_kills_the_line() {
        let mut app = App::default();
        app.begin_command_palette();
        for ch in "focus".chars() {
            app.push_prompt_char(ch);
        }
        let store = test_state_store("ctrl-k");
        let mut pty_runtime = PtyRuntime::new_offline();
        let config = Config::default();
        let layout = AppLayout::compute(&app, Rect::new(0, 0, 120, 40));

        handle_command_palette_key(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            layout,
        );

        assert_eq!(
            app.prompt_input().unwrap().as_str(),
            "focus",
            "ctrl-k must not delete anything"
        );
        let Some(Prompt::CommandPalette(prompt)) = app.prompt() else {
            panic!("palette is open");
        };
        let len = app.active_command_palette_entries().len();
        assert_eq!(
            prompt.selected.index(),
            len - 1,
            "ctrl-k wraps to the last entry"
        );

        // ...and it means the same nothing-destructive thing in a prompt with
        // no list at all.
        let mut app = App::default();
        assert!(app.begin_search());
        for ch in "needle".chars() {
            app.push_prompt_char(ch);
        }
        handle_search_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.prompt_input().unwrap().as_str(), "needle");
    }

    #[test]
    fn the_palette_can_open_the_overlay_and_ask_for_a_config_reload() {
        let store = test_state_store("help-palette");
        let config = Config::default();
        let mut app = App::default();
        let layout = AppLayout::compute(&app, Rect::new(0, 0, 120, 40));
        let mut pty_runtime = PtyRuntime::new_offline();

        execute_command_action(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            CommandAction::ShowKeybindings,
            layout,
        );
        assert!(app.is_help_visible());

        execute_command_action(
            &mut app,
            &mut pty_runtime,
            &config,
            &store,
            CommandAction::ReloadConfig,
            layout,
        );
        // The action only records the request; the loop, which owns the
        // `Config`, performs the swap (E9).
        assert!(app.take_config_reload_request());
    }
}
