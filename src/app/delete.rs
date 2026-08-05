//! Deleting what is selected (E3).
//!
//! Asking is the default; the confirmation is skipped only when the item is
//! *provably* empty, and anything that would also remove the parent workspace
//! is always confirmed — that cascade is the part of the old behaviour nobody
//! asked for, and it used to happen silently.

use super::{App, ConfirmDeletePrompt, NavItem, Prompt};
use crate::model::{ChatId, ChatStatus, PtyKey, TerminalId, WorkspaceId};

/// What [`App::request_delete_selected`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteRequest {
    /// Nothing selected that can be deleted.
    Nothing,
    /// The item was provably empty, so it went without asking. Carries the PTYs
    /// the caller still has to stop.
    Deleted(Vec<PtyKey>),
    /// A confirmation prompt is open; nothing has changed yet.
    Confirming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteTarget {
    Workspace(WorkspaceId),
    Chat {
        workspace: WorkspaceId,
        chat: ChatId,
    },
    Terminal {
        workspace: WorkspaceId,
        terminal: TerminalId,
    },
}

impl App {
    pub fn selected_item_can_be_deleted(&self) -> bool {
        self.selected_delete_target().is_some()
    }

    /// Start a delete of the selected item (E3).
    ///
    /// `pty_has_content` is what the caller can see and this module cannot: the
    /// selected item's PTY is running, or its screen still holds output. The
    /// confirmation is skipped only when the item is *provably* empty by that
    /// plus the durable state — see [`App::delete_needs_confirmation`] — so the
    /// rule is one predicate, not a feel.
    pub fn request_delete_selected(&mut self, pty_has_content: bool) -> DeleteRequest {
        let Some(target) = self.selected_delete_target() else {
            return DeleteRequest::Nothing;
        };

        if !self.delete_needs_confirmation(target, pty_has_content) {
            self.close_prompt();
            return DeleteRequest::Deleted(self.delete_target(target));
        }

        let Some(summary) = self.delete_target_summary(target) else {
            return DeleteRequest::Nothing;
        };
        self.open_prompt(Prompt::ConfirmDelete(ConfirmDeletePrompt {
            target,
            summary,
            cascade: self.delete_cascade_summary(target),
        }));
        DeleteRequest::Confirming
    }

    /// Carry out the delete the open confirmation prompt describes.
    pub fn confirm_delete(&mut self) -> Vec<PtyKey> {
        let Some(Prompt::ConfirmDelete(prompt)) = self.mode.prompt() else {
            return Vec::new();
        };
        let target = prompt.target;
        self.close_prompt();
        self.delete_target(target)
    }

    /// Whether `target` is worth asking about.
    ///
    /// Asking is the default; the exceptions are narrow and checkable:
    ///
    /// * a chat whose agent PTY has no content and whose status is idle;
    /// * a shell terminal (no configured command) that is not running and whose
    ///   screen is blank.
    ///
    /// Anything that would also remove the parent workspace is always confirmed
    /// — that cascade is the part of the old behaviour the user never asked for.
    fn delete_needs_confirmation(&self, target: DeleteTarget, pty_has_content: bool) -> bool {
        if self.delete_cascade_summary(target).is_some() {
            return true;
        }

        match target {
            // A workspace is something the user opened by hand; losing its name
            // and path is a change worth one keypress, empty or not.
            DeleteTarget::Workspace(_) => true,
            DeleteTarget::Chat { workspace, chat } => {
                pty_has_content
                    || self
                        .project
                        .chat(workspace, chat)
                        .is_none_or(|chat| chat.status != ChatStatus::Idle)
            }
            DeleteTarget::Terminal {
                workspace,
                terminal,
            } => {
                pty_has_content
                    || self
                        .project
                        .terminal(workspace, terminal)
                        .is_none_or(|terminal| {
                            !matches!(terminal.launch, crate::model::TerminalLaunch::Shell)
                                || terminal.restore_on_launch
                        })
            }
        }
    }

    /// What the confirmation says is about to be deleted.
    fn delete_target_summary(&self, target: DeleteTarget) -> Option<String> {
        match target {
            DeleteTarget::Workspace(workspace) => {
                let workspace = self.project.workspace(workspace)?;
                Some(format!("empty workspace \"{}\"", workspace.name))
            }
            DeleteTarget::Chat { workspace, chat } => {
                let workspace_name = &self.project.workspace(workspace)?.name;
                let chat = self.project.chat(workspace, chat)?;
                Some(format!(
                    "chat \"{}\" in workspace \"{workspace_name}\"",
                    chat.name
                ))
            }
            DeleteTarget::Terminal {
                workspace,
                terminal,
            } => {
                let workspace_name = &self.project.workspace(workspace)?.name;
                let terminal = self.project.terminal(workspace, terminal)?;
                let command = match &terminal.launch {
                    crate::model::TerminalLaunch::Command(command) => format!(" ({command})"),
                    crate::model::TerminalLaunch::Shell => String::new(),
                };
                Some(format!(
                    "terminal \"{}\"{command} in workspace \"{workspace_name}\"",
                    terminal.name
                ))
            }
        }
    }

    /// The workspace `remove_workspace_if_empty` would take with this delete,
    /// described, or `None` when the workspace survives.
    fn delete_cascade_summary(&self, target: DeleteTarget) -> Option<String> {
        let (workspace_id, chats, terminals) = match target {
            DeleteTarget::Workspace(_) => return None,
            DeleteTarget::Chat { workspace, .. } => (workspace, 1, 0),
            DeleteTarget::Terminal { workspace, .. } => (workspace, 0, 1),
        };
        let workspace = self.project.workspace(workspace_id)?;
        if workspace.chats.len() != chats || workspace.terminals.len() != terminals {
            return None;
        }
        Some(format!(
            "workspace \"{}\" holds nothing else and is removed with it",
            workspace.name
        ))
    }

    fn delete_target(&mut self, target: DeleteTarget) -> Vec<PtyKey> {
        // Remember where the selection sat so that, if the selected item is the
        // one being removed, the selection lands on whatever shifts into its slot.
        let previous_index = self.selected_index();
        let mut runtime_terminals = Vec::new();
        match target {
            DeleteTarget::Workspace(workspace_id) => {
                let can_remove = self
                    .project
                    .workspace(workspace_id)
                    .is_some_and(|workspace| {
                        workspace.chats.is_empty() && workspace.terminals.is_empty()
                    });
                if can_remove && self.project.remove_workspace(workspace_id).is_some() {
                    self.workspace_git_branches.remove(&workspace_id);
                    self.mark_structural_change();
                }
            }
            DeleteTarget::Chat { workspace, chat } => {
                if self.project.remove_chat(workspace, chat).is_some() {
                    runtime_terminals.push(PtyKey::ChatAgent(chat));
                    self.mark_structural_change();
                    self.remove_workspace_if_empty(workspace);
                }
            }
            DeleteTarget::Terminal {
                workspace,
                terminal,
            } => {
                if self.project.remove_terminal(workspace, terminal).is_some() {
                    runtime_terminals.push(PtyKey::Terminal(terminal));
                    self.mark_structural_change();
                    self.remove_workspace_if_empty(workspace);
                }
            }
        }

        self.reconcile_selection(previous_index);
        self.sync_focus_to_selection();
        runtime_terminals
    }

    fn selected_delete_target(&self) -> Option<DeleteTarget> {
        match self.selected_item() {
            Some(NavItem::Chat { workspace, chat }) => Some(DeleteTarget::Chat { workspace, chat }),
            Some(NavItem::Terminal {
                workspace,
                terminal,
            }) => Some(DeleteTarget::Terminal {
                workspace,
                terminal,
            }),
            None => self.first_empty_workspace_id().map(DeleteTarget::Workspace),
        }
    }

    fn first_empty_workspace_id(&self) -> Option<WorkspaceId> {
        self.project
            .workspaces
            .iter()
            .find(|workspace| workspace.chats.is_empty() && workspace.terminals.is_empty())
            .map(|workspace| workspace.id)
    }

    fn remove_workspace_if_empty(&mut self, workspace_id: WorkspaceId) {
        let is_empty = self
            .project
            .workspace(workspace_id)
            .is_some_and(|workspace| workspace.chats.is_empty() && workspace.terminals.is_empty());
        if !is_empty {
            return;
        }

        if self.project.remove_workspace(workspace_id).is_some() {
            self.workspace_git_branches.remove(&workspace_id);
            self.mark_structural_change();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProjectState;

    /// A workspace with two shell terminals, so deleting one does not also
    /// remove the workspace — the case where the confirmation rule is about the
    /// item itself and nothing else.
    fn app_with_two_terminals() -> (App, WorkspaceId, TerminalId, TerminalId) {
        let mut project = ProjectState::two_workspaces();
        project.workspaces.clear();
        let workspace = project.add_workspace("orbit".to_string(), None);
        let first = project
            .add_terminal(workspace, "shell".to_string(), false)
            .expect("add terminal");
        let second = project
            .add_terminal(workspace, "other".to_string(), false)
            .expect("add terminal");
        (App::new(project), workspace, first, second)
    }

    #[test]
    fn deleting_a_terminal_with_output_asks_before_touching_anything() {
        let (mut app, workspace, terminal, _) = app_with_two_terminals();
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        app.mark_clean();

        // `true` = the PTY is running or its screen still holds output.
        assert_eq!(app.request_delete_selected(true), DeleteRequest::Confirming);

        let Some(Prompt::ConfirmDelete(prompt)) = app.prompt() else {
            panic!("expected a confirmation prompt, got {:?}", app.prompt());
        };
        assert!(prompt.summary.contains("terminal \"shell\""), "{prompt:?}");
        assert!(prompt.summary.contains("workspace \"orbit\""), "{prompt:?}");
        assert!(prompt.cascade.is_none(), "{prompt:?}");
        assert!(app.project.terminal(workspace, terminal).is_some());
        assert!(!app.is_dirty());
    }

    #[test]
    fn cancelling_the_confirmation_leaves_everything_in_place() {
        let (mut app, workspace, terminal, _) = app_with_two_terminals();
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        app.mark_clean();
        let before = app.project.clone();

        app.request_delete_selected(true);
        app.cancel_prompt();

        assert_eq!(app.prompt(), None);
        assert_eq!(app.project, before);
        assert!(!app.is_dirty());
    }

    #[test]
    fn confirming_deletes_the_terminal_and_hands_back_its_pty() {
        let (mut app, workspace, terminal, _) = app_with_two_terminals();
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });

        app.request_delete_selected(true);
        let runtime_terminals = app.confirm_delete();

        assert_eq!(runtime_terminals, vec![PtyKey::Terminal(terminal)]);
        assert!(app.project.terminal(workspace, terminal).is_none());
        assert_eq!(app.prompt(), None);
        assert!(app.is_dirty());
    }

    #[test]
    fn an_untouched_shell_terminal_is_deleted_without_a_confirmation() {
        let (mut app, workspace, terminal, _) = app_with_two_terminals();
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });

        // Stopped, no configured command, blank screen: nothing to lose, and
        // the workspace keeps its second terminal.
        assert_eq!(
            app.request_delete_selected(false),
            DeleteRequest::Deleted(vec![PtyKey::Terminal(terminal)])
        );
        assert_eq!(app.prompt(), None);
        assert!(app.project.terminal(workspace, terminal).is_none());
    }

    #[test]
    fn a_command_terminal_is_never_deleted_without_asking() {
        let (mut app, workspace, _, _) = app_with_two_terminals();
        let terminal = app
            .project
            .add_command_terminal(workspace, "dev".to_string(), false, "cargo run".to_string())
            .expect("add command terminal");
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });

        assert_eq!(
            app.request_delete_selected(false),
            DeleteRequest::Confirming
        );
        let Some(Prompt::ConfirmDelete(prompt)) = app.prompt() else {
            panic!("expected a confirmation prompt");
        };
        // The command is what a stopped command terminal is; naming it is the
        // difference between "delete terminal 3?" and a real question.
        assert!(prompt.summary.contains("cargo run"), "{prompt:?}");
    }

    #[test]
    fn delete_selected_chat_removes_it_and_names_its_agent_pty() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });
        let pi_terminal = PtyKey::ChatAgent(chat);

        assert_eq!(app.request_delete_selected(true), DeleteRequest::Confirming);
        let runtime_terminals = app.confirm_delete();

        assert_eq!(runtime_terminals, vec![pi_terminal]);
        assert!(app.project.chat(workspace, chat).is_none());
        assert!(app.is_dirty());
    }

    /// A busy chat is always confirmed, and the confirmation names it. It used
    /// to also count the chat's persisted messages, which nothing ever wrote (F1).
    #[test]
    fn a_busy_chat_is_confirmed_and_the_confirmation_names_it() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });
        app.mark_chat_status_by_id(chat, ChatStatus::Thinking);

        assert_eq!(
            app.request_delete_selected(false),
            DeleteRequest::Confirming
        );
        let Some(Prompt::ConfirmDelete(prompt)) = app.prompt() else {
            panic!("expected a confirmation prompt");
        };
        assert!(prompt.summary.contains("chat \""), "{prompt:?}");
    }

    #[test]
    fn an_idle_chat_with_no_transcript_is_deleted_without_a_confirmation() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });

        assert_eq!(
            app.request_delete_selected(false),
            DeleteRequest::Deleted(vec![PtyKey::ChatAgent(chat)])
        );
        assert_eq!(app.prompt(), None);
    }

    #[test]
    fn deleting_the_selected_item_selects_the_successor_in_its_slot() {
        let mut app = App::two_workspaces();
        let items = app.nav_items();
        assert!(
            items.len() >= 2,
            "default seed should have multiple nav items"
        );

        app.select_item(items[0]);
        assert_eq!(app.selected_item(), Some(items[0]));

        app.request_delete_selected(true);
        app.confirm_delete();

        // The item that shifts into the vacated slot becomes selected (the old
        // second item), matching the position-stable behavior.
        assert_eq!(app.selected_item(), Some(items[1]));
    }

    #[test]
    fn deleting_last_workspace_item_announces_the_workspace_going_with_it() {
        let mut app = App::seeded();
        app.project.workspaces.truncate(1);
        app.project.workspaces[0].terminals.clear();
        app.project.workspaces[0].chats.truncate(1);
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });

        // Empty and idle, so the item alone would not be worth asking about —
        // but the workspace disappearing with it is (E3).
        assert_eq!(
            app.request_delete_selected(false),
            DeleteRequest::Confirming
        );
        let Some(Prompt::ConfirmDelete(prompt)) = app.prompt() else {
            panic!("expected a confirmation prompt, got {:?}", app.prompt());
        };
        let cascade = prompt.cascade.clone().expect("cascade is announced");
        assert!(cascade.contains("workspace \"mult\""), "{cascade}");
        assert!(cascade.contains("removed with it"), "{cascade}");

        let runtime_terminals = app.confirm_delete();

        assert_eq!(runtime_terminals, vec![PtyKey::ChatAgent(chat)]);
        assert!(app.project.workspace(workspace).is_none());
        assert!(app.is_dirty());
    }

    #[test]
    fn empty_workspace_can_be_closed_without_selecting_workspace() {
        let mut app = App::two_workspaces();
        app.project.workspaces.truncate(1);
        app.project.workspaces[0].chats.clear();
        app.project.workspaces[0].terminals.clear();
        let workspace = app.project.workspaces[0].id;
        app.reconcile_selection(None);

        assert_eq!(
            app.request_delete_selected(false),
            DeleteRequest::Confirming
        );
        let runtime_terminals = app.confirm_delete();

        assert!(runtime_terminals.is_empty());
        assert!(app.project.workspace(workspace).is_none());
        assert!(app.is_dirty());
    }
}
