//! Structural changes to the project: adding and deleting workspaces, chats and
//! terminals, and the per-item runtime state those changes have to keep honest.

use mult_protocol::shell::quote_argument;

use crate::model::{
    AgentGeneration, AgentKind, ChatId, ChatStatus, PtyKey, TerminalId, TerminalLaunch,
    WorkspaceId, DEFAULT_AGENT_CHAT_TITLE,
};

use super::*;

const SFTP_TERMINAL_NAME: &str = "sftp";
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

impl DeleteTarget {
    /// The PTY this target owns, if any. A workspace is deletable only once it
    /// holds no chats and no terminals, so it never owns one itself.
    pub fn pty_key(self) -> Option<PtyKey> {
        match self {
            Self::Workspace(_) => None,
            Self::Chat { chat, .. } => Some(PtyKey::ChatAgent(chat)),
            Self::Terminal { terminal, .. } => Some(PtyKey::Terminal(terminal)),
        }
    }
}

impl App {
    /// Removes `target` and reports the PTYs whose durable state went with it.
    ///
    /// Deletion is immediate — there is no confirmation step. Callers that own
    /// a `PtyRuntime` stop the target's PTY *before* calling this, because this
    /// mutates durable state and must not run ahead of the daemon (see
    /// `runtime::prompt::delete_selected`).
    pub fn delete_target(&mut self, target: DeleteTarget) -> Vec<PtyKey> {
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
                    self.chat_buffers.remove(&chat);
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
                    self.recoverable_terminals.remove(&terminal);
                    self.mark_structural_change();
                    self.remove_workspace_if_empty(workspace);
                }
            }
        }

        self.reconcile_selection(previous_index);
        self.sync_focus_to_selection();
        runtime_terminals
    }

    pub fn selected_delete_target(&self) -> Option<DeleteTarget> {
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

    /// Removes a terminal whose program finished on its own, reporting the
    /// PTYs whose durable state went with it the way [`App::delete_target`]
    /// does.
    ///
    /// Unlike a deletion there is no stop to order this against: the child is
    /// already gone and the daemon dropped its session when it reaped it, so
    /// the only thing left is the row standing for it. Emptying the workspace
    /// this way retires it, exactly as deleting the last pane by hand does.
    pub fn remove_finished_terminal(&mut self, terminal: TerminalId) -> Vec<PtyKey> {
        let Some(workspace) = self.workspace_of_terminal(terminal) else {
            return Vec::new();
        };

        self.delete_target(DeleteTarget::Terminal {
            workspace,
            terminal,
        })
    }

    fn workspace_of_terminal(&self, terminal: TerminalId) -> Option<WorkspaceId> {
        self.project
            .workspaces
            .iter()
            .find(|workspace| workspace.terminals.iter().any(|item| item.id == terminal))
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

    /// Records that this terminal is attached, so the next launch should bring
    /// it back.
    ///
    /// This persists *intent*, not liveness: whether the pane is live right now
    /// is `PtyRuntime`'s to answer and nothing else's (F16). Losing this call
    /// therefore costs a restoration, not a terminal stuck "running" forever.
    pub fn record_terminal_started(&mut self, terminal: TerminalId) {
        self.recoverable_terminals.remove(&terminal);
        self.set_terminal_restore_intent(terminal, true);
    }

    pub fn mark_terminal_recoverable(&mut self, terminal: TerminalId) {
        if self
            .project
            .terminal_mut_by_id(terminal)
            .is_some_and(|terminal| {
                matches!(terminal.launch, crate::model::TerminalLaunch::Command(_))
            })
        {
            self.recoverable_terminals.insert(terminal);
        }
    }

    pub fn terminal_requires_recovery(&self, terminal: TerminalId) -> bool {
        self.recoverable_terminals.contains(&terminal)
    }

    /// Records that this terminal is not attached, so the next launch leaves it
    /// alone until the user asks. The counterpart of
    /// [`Self::record_terminal_started`].
    pub fn record_terminal_stopped(&mut self, terminal: TerminalId) {
        self.set_terminal_restore_intent(terminal, false);
    }

    fn set_terminal_restore_intent(&mut self, terminal: TerminalId, restore: bool) {
        if let Some(terminal) = self.project.terminal_mut_by_id(terminal) {
            if terminal.restore_on_launch != restore {
                terminal.restore_on_launch = restore;
                self.dirty = true;
            }
        }
    }

    pub fn begin_agent_generation(
        &mut self,
        chat: ChatId,
    ) -> Result<Option<AgentGeneration>, crate::model::IdAllocationError> {
        let generation = self.project.begin_agent_generation(chat)?;
        if generation.is_some() {
            self.dirty = true;
        }
        Ok(generation)
    }

    pub fn clear_agent_generation(&mut self, chat: ChatId, generation: AgentGeneration) -> bool {
        let changed = self.project.clear_agent_generation(chat, generation);
        self.dirty |= changed;
        changed
    }

    /// Sets a chat's status, resolving the seen bit in the same assignment.
    /// Returns whether anything changed (useful for deciding if a redraw is
    /// needed).
    ///
    /// A chat that finishes while the user is already looking at it counts as
    /// seen immediately, so it never flashes as an unseen notification;
    /// finishing in the background arms one; leaving the finished state drops
    /// the bit, so re-prompting a finished agent arms a fresh notification.
    /// Callers pass the plain status they observed — the seen bit is derived
    /// here and never theirs to decide, which is what makes the two
    /// impossible to fall out of step (F16).
    pub fn mark_chat_status_by_id(&mut self, chat: ChatId, status: ChatStatus) -> bool {
        let seen = self.selected_chat_id().map(|(_, chat)| chat) == Some(chat);
        let status = status.with_done_seen(seen);
        let mut changed = false;
        for chat_session in self
            .project
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.chats.iter_mut())
        {
            if chat_session.id == chat {
                if chat_session.status != status {
                    chat_session.status = status;
                    self.dirty = true;
                    changed = true;
                }
                break;
            }
        }
        changed
    }

    pub fn add_chat_to_selected_workspace_and_return(
        &mut self,
        agent: AgentKind,
    ) -> Option<(WorkspaceId, ChatId)> {
        let workspace = self.selected_workspace_id()?;
        // A remote workspace holds one agent, because it has one `tmux` session
        // — named after the project — and a second chat on that name would be
        // the first agent mirrored into another pane rather than a new one. So
        // the key that would add a chat selects the chat already there; the
        // caller then starts or focuses it, which for an agent already running
        // over there is a re-attach.
        if let Some(existing) = self.only_remote_chat(workspace) {
            self.select_item(NavItem::Chat {
                workspace,
                chat: existing,
            });
            self.clear_operation_error();
            return Some((workspace, existing));
        }
        let name = DEFAULT_AGENT_CHAT_TITLE.to_string();
        let chat = match self
            .project
            .add_chat(workspace, name, ChatStatus::Idle, agent)
        {
            Ok(Some(chat)) => chat,
            Ok(None) => {
                self.record_operation_failure("selected workspace no longer exists");
                return None;
            }
            Err(error) => {
                self.record_operation_failure(error.to_string());
                return None;
            }
        };
        self.select_item(NavItem::Chat { workspace, chat });
        self.clear_operation_error();
        self.mark_structural_change();
        Some((workspace, chat))
    }

    pub fn add_terminal_to_selected_workspace(&mut self) {
        let Some(workspace) = self.selected_workspace_id() else {
            return;
        };
        let next = self
            .project
            .workspace(workspace)
            .map(|workspace| workspace.terminals.len() + 1)
            .unwrap_or(1);

        let name = format!("terminal-{next}");
        match self.project.add_terminal(workspace, name.clone()) {
            Ok(Some(terminal)) => {
                self.select_item(NavItem::Terminal {
                    workspace,
                    terminal,
                });
                self.clear_operation_error();
                self.mark_structural_change();
            }
            Ok(None) => self.record_operation_failure("selected workspace no longer exists"),
            Err(error) => self.record_operation_failure(error.to_string()),
        }
    }

    /// Selects the file manager terminal of the selected workspace, adding one
    /// that runs `command` in the workspace root if there is none yet.
    pub fn open_file_manager_in_selected_workspace(&mut self, command: &str) -> Option<TerminalId> {
        self.open_tool_in_selected_workspace(command, "no file manager command is configured")
    }

    /// The same, for the editor `Ctrl+e` opens.
    ///
    /// The workspace keeps one editor for the same reason it keeps one file
    /// manager: the key is "show me my editor here", not "give me another one".
    pub fn open_editor_in_selected_workspace(&mut self, command: &str) -> Option<TerminalId> {
        self.open_tool_in_selected_workspace(command, "no editor command is configured")
    }

    /// Selects this workspace's Yazi SFTP terminal, adding it when needed.
    ///
    /// SFTP is identified by the reserved terminal name rather than by the
    /// generated command. A config reload may change the target, but an open
    /// workspace must still never acquire a second SFTP pane alongside the
    /// first one. Once that pane exits cleanly normal terminal lifecycle removes
    /// it, and the next press opens the newly configured target.
    pub fn open_sftp_in_selected_workspace(&mut self, target: Option<&str>) -> Option<TerminalId> {
        let workspace = self.selected_workspace_id()?;

        if let Some(terminal) = self
            .project
            .workspace(workspace)?
            .terminals
            .iter()
            .find(|terminal| {
                terminal.name == SFTP_TERMINAL_NAME
                    && matches!(
                        &terminal.launch,
                        TerminalLaunch::Command(command) if is_yazi_sftp_command(command)
                    )
            })
            .map(|terminal| terminal.id)
        {
            self.select_item(NavItem::Terminal {
                workspace,
                terminal,
            });
            self.clear_operation_error();
            return Some(terminal);
        }

        let Some(target) = target.map(str::trim).filter(|target| !target.is_empty()) else {
            self.record_operation_failure("no SFTP target is configured for this workspace");
            return None;
        };
        let url = if target.starts_with("sftp://") {
            target.to_string()
        } else {
            format!("sftp://{target}")
        };
        let command = format!("yazi {}", quote_argument(&url));
        match self
            .project
            .add_command_terminal(workspace, SFTP_TERMINAL_NAME.to_string(), command)
        {
            Ok(Some(terminal)) => {
                self.select_item(NavItem::Terminal {
                    workspace,
                    terminal,
                });
                self.clear_operation_error();
                self.mark_structural_change();
                Some(terminal)
            }
            Ok(None) => {
                self.record_operation_failure("selected workspace no longer exists");
                None
            }
            Err(error) => {
                self.record_operation_failure(error.to_string());
                None
            }
        }
    }

    /// Selects the terminal of the selected workspace running `command`, adding
    /// one in the workspace root if there is none yet.
    ///
    /// A file manager or an editor is an ordinary command terminal, so it is
    /// recognised by its launch command rather than by a flag of its own:
    /// pressing the key again lands on the pane the first press made instead of
    /// stacking a second one beside it. Starting the PTY is the caller's job —
    /// this only touches durable state.
    fn open_tool_in_selected_workspace(
        &mut self,
        command: &str,
        missing: &str,
    ) -> Option<TerminalId> {
        let command = command.trim();
        if command.is_empty() {
            self.record_operation_failure(missing);
            return None;
        }
        let workspace = self.selected_workspace_id()?;

        if let Some(terminal) = self.command_terminal_running(workspace, command) {
            self.select_item(NavItem::Terminal {
                workspace,
                terminal,
            });
            self.clear_operation_error();
            return Some(terminal);
        }

        match self
            .project
            .add_command_terminal(workspace, command.to_string(), command.to_string())
        {
            Ok(Some(terminal)) => {
                self.select_item(NavItem::Terminal {
                    workspace,
                    terminal,
                });
                self.clear_operation_error();
                self.mark_structural_change();
                Some(terminal)
            }
            Ok(None) => {
                self.record_operation_failure("selected workspace no longer exists");
                None
            }
            Err(error) => {
                self.record_operation_failure(error.to_string());
                None
            }
        }
    }

    /// The chat a remote workspace already has, if it is remote and it has one.
    fn only_remote_chat(&self, workspace: WorkspaceId) -> Option<ChatId> {
        let workspace = self.project.workspace(workspace)?;
        workspace.remote.as_ref()?;
        workspace.chats.first().map(|chat| chat.id)
    }

    fn command_terminal_running(
        &self,
        workspace: WorkspaceId,
        command: &str,
    ) -> Option<TerminalId> {
        self.project
            .workspace(workspace)?
            .terminals
            .iter()
            .find(|terminal| match &terminal.launch {
                crate::model::TerminalLaunch::Command(existing) => existing == command,
                crate::model::TerminalLaunch::Shell => false,
            })
            .map(|terminal| terminal.id)
    }
}

fn is_yazi_sftp_command(command: &str) -> bool {
    command
        .strip_prefix("yazi ")
        .is_some_and(|target| target.starts_with("sftp://") || target.starts_with("'sftp://"))
}

pub(super) fn clean_git_branch_name(branch: String) -> Option<String> {
    let branch = branch.trim();
    (!branch.is_empty()).then(|| branch.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sftp_command_preserves_a_full_url_as_one_shell_argument() {
        let mut app = App::default();
        let workspace = app.selected_workspace_id().unwrap();

        let terminal = app
            .open_sftp_in_selected_workspace(Some(
                "sftp://my-server//work space;$HOME`not-a-command`",
            ))
            .expect("an SFTP terminal is added");

        assert_eq!(
            app.project.terminal(workspace, terminal).unwrap().launch,
            TerminalLaunch::Command(
                "yazi 'sftp://my-server//work space;$HOME`not-a-command`'".to_string()
            )
        );
        assert!(!is_yazi_sftp_command("sftp"));
    }

    #[test]
    fn each_workspace_keeps_its_own_sftp_terminal() {
        let mut app = App::default();
        let first_workspace = app.project.workspaces[0].id;
        let first = app
            .open_sftp_in_selected_workspace(Some("first"))
            .expect("the first workspace gets an SFTP terminal");
        let second_workspace = app.project.workspaces[1].id;
        let second_shell = app.project.workspaces[1].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace: second_workspace,
            terminal: second_shell,
        });

        let second = app
            .open_sftp_in_selected_workspace(Some("second"))
            .expect("the second workspace gets its own SFTP terminal");

        assert_ne!(first, second);
        assert_eq!(
            app.project
                .workspace(first_workspace)
                .unwrap()
                .terminals
                .len(),
            2
        );
        assert_eq!(
            app.project
                .workspace(second_workspace)
                .unwrap()
                .terminals
                .len(),
            2
        );
    }

    #[test]
    fn new_chats_use_agent_title() {
        let mut app = App::default();
        let Some((workspace, chat)) = app.add_chat_to_selected_workspace_and_return(AgentKind::Pi)
        else {
            panic!("chat should be added");
        };

        assert_eq!(
            app.project.chat(workspace, chat).unwrap().name,
            DEFAULT_AGENT_CHAT_TITLE
        );
    }

    /// A remote workspace has one `tmux` session, named for the project, so it
    /// has one chat: pressing the key again lands on the chat already there
    /// rather than adding a second pane onto the same agent.
    #[test]
    fn a_remote_workspace_keeps_one_agent_chat_and_returns_to_it() {
        let mut app = App::default();
        let workspace = app.selected_workspace_id().unwrap();
        app.project.workspace_mut(workspace).unwrap().chats.clear();
        app.project.workspace_mut(workspace).unwrap().remote = Some(crate::model::RemoteTarget {
            host: "user@hostname".to_string(),
            path: "~/projects/mult".to_string(),
            session: "mult".to_string(),
        });
        let terminal = app.project.workspace(workspace).unwrap().terminals[0].id;

        let (_, first) = app
            .add_chat_to_selected_workspace_and_return(AgentKind::ClaudeCode)
            .expect("the first chat is created");
        // Somewhere else in the same workspace, so the second press has to
        // navigate rather than already being there.
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });

        let (_, again) = app
            .add_chat_to_selected_workspace_and_return(AgentKind::Pi)
            .expect("the key returns the chat already there");

        assert_eq!(again, first, "the same chat, whichever agent key was used");
        assert_eq!(
            app.project.workspace(workspace).unwrap().chats.len(),
            1,
            "and no second one beside it"
        );
        assert_eq!(
            app.selected_item(),
            Some(NavItem::Chat {
                workspace,
                chat: first
            }),
            "pressing it again navigates to the open chat"
        );
    }

    /// A local workspace is unchanged: every press is another chat.
    #[test]
    fn a_local_workspace_still_adds_a_chat_each_time() {
        let mut app = App::default();
        let workspace = app.selected_workspace_id().unwrap();
        let chats = app.project.workspace(workspace).unwrap().chats.len();

        app.add_chat_to_selected_workspace_and_return(AgentKind::Pi);
        app.add_chat_to_selected_workspace_and_return(AgentKind::ClaudeCode);

        assert_eq!(
            app.project.workspace(workspace).unwrap().chats.len(),
            chats + 2
        );
    }

    #[test]
    fn a_remote_workspace_still_adds_terminals() {
        let mut app = App::default();
        let workspace = app.selected_workspace_id().unwrap();
        app.project.workspace_mut(workspace).unwrap().remote = Some(crate::model::RemoteTarget {
            host: "user@hostname".to_string(),
            path: "~/projects/mult".to_string(),
            session: "mult".to_string(),
        });
        let terminals = app.project.workspace(workspace).unwrap().terminals.len();

        app.add_terminal_to_selected_workspace();

        assert_eq!(
            app.project.workspace(workspace).unwrap().terminals.len(),
            terminals + 1
        );
    }

    #[test]
    fn new_chats_record_their_agent_kind() {
        let mut app = App::default();
        let Some((workspace, chat)) =
            app.add_chat_to_selected_workspace_and_return(AgentKind::ClaudeCode)
        else {
            panic!("chat should be added");
        };

        assert_eq!(
            app.project.chat(workspace, chat).unwrap().agent,
            AgentKind::ClaudeCode
        );
    }

    #[test]
    fn persisted_stopped_command_requires_deliberate_recovery_until_running() {
        let mut state = ProjectState::try_first_run().expect("first-run project");
        let terminal = state.workspaces[0].terminals[0].id;
        state.workspaces[0].terminals[0].launch =
            crate::model::TerminalLaunch::Command("cargo test".to_string());
        state.workspaces[0].terminals[0].restore_on_launch = false;

        let mut app = App::new(state);
        assert!(app.terminal_requires_recovery(terminal));

        app.record_terminal_started(terminal);
        assert!(!app.terminal_requires_recovery(terminal));
    }

    #[test]
    fn deleting_the_selected_terminal_removes_it_outright() {
        let mut app = App::default();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });

        let target = app.selected_delete_target().expect("a target is selected");
        assert_eq!(target.pty_key(), Some(PtyKey::Terminal(terminal)));

        let runtime_terminals = app.delete_target(target);
        assert_eq!(runtime_terminals, vec![PtyKey::Terminal(terminal)]);
        assert!(app.project.terminal(workspace, terminal).is_none());
        assert!(app.is_dirty());
    }

    /// With nothing selected and no empty workspace to close, there is nothing
    /// for `Ctrl+q` to delete — and, now that the key deletes immediately, that
    /// has to be a no-op rather than a nearby item.
    #[test]
    fn nothing_deletable_selects_no_target() {
        let mut app = App::default();
        app.project.workspaces.clear();
        app.reconcile_selection(None);

        assert_eq!(app.selected_delete_target(), None);
    }

    #[test]
    fn delete_selected_chat_removes_transcript_and_pi_runtime_id() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });
        app.chat_buffers.insert(chat, ChatBuffer::default());
        let pi_terminal = PtyKey::ChatAgent(chat);
        let target = app.selected_delete_target().expect("a target is selected");
        let runtime_terminals = app.delete_target(target);

        assert_eq!(runtime_terminals, vec![pi_terminal]);
        assert!(app.project.chat(workspace, chat).is_none());
        assert!(!app.chat_buffers.contains_key(&chat));
        assert!(app.is_dirty());
    }

    #[test]
    fn deleting_last_workspace_item_closes_workspace() {
        let mut app = App::seeded();
        app.project.workspaces.truncate(1);
        app.project.workspaces[0].terminals.clear();
        app.project.workspaces[0].chats.truncate(1);
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });

        let target = app.selected_delete_target().expect("a target is selected");
        let runtime_terminals = app.delete_target(target);

        assert_eq!(runtime_terminals, vec![PtyKey::ChatAgent(chat)]);
        assert!(app.project.workspace(workspace).is_none());
        assert!(app.is_dirty());
    }

    #[test]
    fn empty_workspace_can_be_closed_without_selecting_workspace() {
        let mut app = App::default();
        app.project.workspaces.truncate(1);
        app.project.workspaces[0].chats.clear();
        app.project.workspaces[0].terminals.clear();
        let workspace = app.project.workspaces[0].id;
        app.reconcile_selection(None);

        let target = app.selected_delete_target().expect("the empty workspace");
        assert_eq!(target.pty_key(), None);
        let runtime_terminals = app.delete_target(target);

        assert!(runtime_terminals.is_empty());
        assert!(app.project.workspace(workspace).is_none());
        assert!(app.is_dirty());
    }

    #[test]
    fn unchanged_status_updates_do_not_mark_dirty() {
        let mut app = App::seeded();
        let terminal = app.project.workspaces[0].terminals[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        let chat_status = app.project.workspaces[0].chats[0].status;
        app.mark_clean();

        app.record_terminal_stopped(terminal);
        app.mark_chat_status_by_id(chat, chat_status);

        assert!(!app.is_dirty());
    }
}
