//! The sidebar order, the selection that walks it, and deleting what is
//! selected.
//!
//! The row order and the navigation order are one walk (`App::sidebar_row_iter`),
//! so "the nth selectable row" means the same thing to the renderer and to
//! `select_next` however the order changes (F14).

use super::{App, FocusMode, PaneFocus};
use crate::model::{ChatId, ChatStatus, PtyKey, TerminalId, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavItem {
    Chat {
        workspace: WorkspaceId,
        chat: ChatId,
    },
    Terminal {
        workspace: WorkspaceId,
        terminal: TerminalId,
    },
}

/// One rendered row of the sidebar, in the order it is drawn.
///
/// The sidebar's row order and the nav order are the same walk, and used to be
/// written twice: `App::nav_iter` produced the selectable items, and `ui.rs`
/// re-walked `workspaces → chats → terminals` by hand to turn a nav index back
/// into a list row, counting the header and separator rows itself. Changing the
/// order in one place moved the highlight onto the wrong row with no compile
/// error (F14). `App` now emits the rows, and `ui` finds the highlight by
/// position in them.
/// Intermediate of [`App::sidebar_row_iter`], before nav indices are assigned.
enum SidebarWalkRow {
    Spacer,
    Workspace(WorkspaceId),
    Nav(NavItem),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarRow {
    /// Blank separator drawn between workspace groups.
    Spacer,
    /// A workspace header. Not selectable.
    Workspace(WorkspaceId),
    /// A selectable item, together with its index in the nav order.
    Nav { index: usize, item: NavItem },
}

impl App {
    pub fn focus_sidebar(&mut self) {
        self.set_browsing_focus(PaneFocus::Sidebar);
    }

    pub fn focus_selected_main(&mut self) -> bool {
        if self.selected_main_focus().is_none() {
            return false;
        }

        self.set_browsing_focus(PaneFocus::Pane);
        true
    }

    pub(super) fn selected_main_focus(&self) -> Option<FocusMode> {
        match self.selected_item()? {
            NavItem::Chat { .. } => Some(FocusMode::Chat),
            NavItem::Terminal { .. } => Some(FocusMode::Terminal),
        }
    }

    pub(super) fn sync_focus_to_selection(&mut self) {
        self.set_browsing_focus(if self.selected_main_focus().is_some() {
            PaneFocus::Pane
        } else {
            PaneFocus::Sidebar
        });
        // Every selection change funnels through here (keyboard nav, mouse,
        // startup, post-delete reconcile), so it is the single place to record
        // that the user is now looking at the selected item: a finished agent
        // the user navigates onto stops being an unseen notification.
        self.mark_selected_done_seen();
    }

    /// Marks the currently selected chat's `Done` state as seen, if it is
    /// finished. A no-op for any other status or when a terminal is selected.
    fn mark_selected_done_seen(&mut self) {
        let Some((workspace, chat)) = self.selected_chat_id() else {
            return;
        };
        if let Some(session) = self.project.chat_mut(workspace, chat) {
            if session.status == (ChatStatus::Done { seen: false }) {
                session.status = ChatStatus::Done { seen: true };
                // Presentation only, and deliberately not persisted, so a
                // change here is not worth a save (see `PersistedChatStatus`).
            }
        }
    }

    pub fn terminal_input_target(&self) -> Option<TerminalId> {
        self.selected_terminal_id().map(|(_, terminal)| terminal)
    }

    /// The PTY that keystrokes and search go to: the selected chat's agent
    /// process, or the selected terminal.
    pub fn pty_input_target(&self) -> Option<PtyKey> {
        match self.selected_item()? {
            NavItem::Chat { chat, .. } => Some(PtyKey::ChatAgent(chat)),
            NavItem::Terminal { terminal, .. } => Some(PtyKey::Terminal(terminal)),
        }
    }

    /// The single walk the sidebar and the navigation order both come from:
    /// a spacer between workspace groups, a header per workspace, then that
    /// workspace's chats followed by its terminals.
    ///
    /// Nav indices are assigned here, at the end, so "the nth selectable row"
    /// means the same thing to the renderer and to `select_next` however the
    /// order changes (F14).
    fn sidebar_row_iter(&self) -> impl Iterator<Item = SidebarRow> + '_ {
        self.project
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(position, workspace)| {
                let chats = workspace.chats.iter().map(move |chat| {
                    SidebarWalkRow::Nav(NavItem::Chat {
                        workspace: workspace.id,
                        chat: chat.id,
                    })
                });
                let terminals = workspace.terminals.iter().map(move |terminal| {
                    SidebarWalkRow::Nav(NavItem::Terminal {
                        workspace: workspace.id,
                        terminal: terminal.id,
                    })
                });
                (position > 0)
                    .then_some(SidebarWalkRow::Spacer)
                    .into_iter()
                    .chain(std::iter::once(SidebarWalkRow::Workspace(workspace.id)))
                    .chain(chats)
                    .chain(terminals)
            })
            .scan(0_usize, |next_index, row| {
                Some(match row {
                    SidebarWalkRow::Spacer => SidebarRow::Spacer,
                    SidebarWalkRow::Workspace(workspace) => SidebarRow::Workspace(workspace),
                    SidebarWalkRow::Nav(item) => {
                        let index = *next_index;
                        *next_index += 1;
                        SidebarRow::Nav { index, item }
                    }
                })
            })
    }

    /// The sidebar navigation order: each workspace's chats followed by its
    /// terminals, across all workspaces. Every nav query
    /// (`nav_items`/`nav_len`/`nav_item_at`/`nav_item_position`) is defined in
    /// terms of this, which is in turn the selectable rows of
    /// [`App::sidebar_row_iter`].
    fn nav_iter(&self) -> impl Iterator<Item = NavItem> + '_ {
        self.sidebar_row_iter().filter_map(|row| match row {
            SidebarRow::Nav { item, .. } => Some(item),
            SidebarRow::Spacer | SidebarRow::Workspace(_) => None,
        })
    }

    pub fn nav_items(&self) -> Vec<NavItem> {
        self.nav_iter().collect()
    }

    /// The sidebar as a list of rows, in the order they are drawn.
    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        self.sidebar_row_iter().collect()
    }

    pub fn nav_len(&self) -> usize {
        self.nav_iter().count()
    }

    pub fn nav_item_at(&self, target_index: usize) -> Option<NavItem> {
        self.nav_iter().nth(target_index)
    }

    pub fn selected_item(&self) -> Option<NavItem> {
        // Validate against the current tree so a selection left stale by a direct
        // project mutation is never observable (it resolves to `None`, exactly as
        // the old index-into-list lookup did when out of range).
        self.selected
            .filter(|item| self.nav_item_position(*item).is_some())
    }

    /// The position of the current selection in the nav list, if any. Used for
    /// rendering (the sidebar highlight) and by `select_next`/`select_previous`.
    pub fn selected_index(&self) -> Option<usize> {
        self.selected.and_then(|item| self.nav_item_position(item))
    }

    /// Select the nav item at `index`, if one exists there (otherwise a no-op).
    pub fn select_nav_index(&mut self, index: usize) {
        if let Some(item) = self.nav_item_at(index) {
            self.select_item(item);
        }
    }

    pub fn selected_workspace_id(&self) -> Option<WorkspaceId> {
        match self.selected_item() {
            Some(NavItem::Chat { workspace, .. }) | Some(NavItem::Terminal { workspace, .. }) => {
                Some(workspace)
            }
            None => self
                .project
                .workspaces
                .first()
                .map(|workspace| workspace.id),
        }
    }

    pub fn selected_terminal_id(&self) -> Option<(WorkspaceId, TerminalId)> {
        match self.selected_item() {
            Some(NavItem::Terminal {
                workspace,
                terminal,
            }) => Some((workspace, terminal)),
            _ => None,
        }
    }

    pub fn selected_chat_id(&self) -> Option<(WorkspaceId, ChatId)> {
        match self.selected_item() {
            Some(NavItem::Chat { workspace, chat }) => Some((workspace, chat)),
            _ => None,
        }
    }

    pub fn selected_item_can_start_input(&self) -> bool {
        self.selected_chat_id().is_some() || self.selected_terminal_id().is_some()
    }

    pub fn selected_item_can_search(&self) -> bool {
        self.selected_search_scope().is_some()
    }

    pub fn select_next(&mut self) {
        let len = self.nav_len();
        if len > 0 {
            let next = self.selected_index().map_or(0, |index| (index + 1) % len);
            self.selected = self.nav_item_at(next);
            self.sync_focus_to_selection();
        }
    }

    pub fn select_previous(&mut self) {
        let len = self.nav_len();
        if len > 0 {
            let previous = self
                .selected_index()
                .map_or(len - 1, |index| index.checked_sub(1).unwrap_or(len - 1));
            self.selected = self.nav_item_at(previous);
            self.sync_focus_to_selection();
        }
    }

    pub fn select_item(&mut self, target: NavItem) {
        if self.nav_item_position(target).is_some() {
            self.selected = Some(target);
        } else {
            self.reconcile_selection(self.selected_index());
        }
        self.sync_focus_to_selection();
    }

    pub(super) fn select_first_item_in_workspace(&mut self, workspace_id: WorkspaceId) -> bool {
        let Some(workspace) = self.project.workspace(workspace_id) else {
            self.reconcile_selection(self.selected_index());
            self.sync_focus_to_selection();
            return false;
        };

        let target = workspace
            .chats
            .first()
            .map(|chat| NavItem::Chat {
                workspace: workspace_id,
                chat: chat.id,
            })
            .or_else(|| {
                workspace
                    .terminals
                    .first()
                    .map(|terminal| NavItem::Terminal {
                        workspace: workspace_id,
                        terminal: terminal.id,
                    })
            });

        if let Some(target) = target {
            self.select_item(target);
            true
        } else {
            self.reconcile_selection(self.selected_index());
            self.sync_focus_to_selection();
            false
        }
    }

    fn nav_item_position(&self, target: NavItem) -> Option<usize> {
        self.nav_iter().position(|item| item == target)
    }

    /// Re-establish the selection invariant after a structural change: keep the
    /// current selection if it still exists, otherwise select the item now at
    /// `preferred_index` (clamped to the list), or `None` when the list is empty.
    /// This replaces the old index clamp and preserves position-stable selection
    /// — e.g. deleting the selected item selects whatever shifts into its slot.
    pub(super) fn reconcile_selection(&mut self, preferred_index: Option<usize>) {
        let len = self.nav_len();
        if len == 0 {
            self.selected = None;
            return;
        }
        if let Some(selected) = self.selected {
            if self.nav_item_position(selected).is_some() {
                return;
            }
        }
        let index = preferred_index.unwrap_or(0).min(len - 1);
        self.selected = self.nav_item_at(index);
    }
    pub fn begin_terminal_input(&mut self) -> bool {
        if self.selected_terminal_id().is_none() {
            return false;
        }

        self.set_browsing_focus(PaneFocus::Pane);
        true
    }

    pub fn begin_chat_agent_input(&mut self) -> bool {
        if self.selected_chat_id().is_none() {
            return false;
        }

        self.set_browsing_focus(PaneFocus::Pane);
        true
    }

    pub fn end_pty_input(&mut self) {
        self.sync_focus_to_selection();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_are_not_selectable_nav_items() {
        let app = App::seeded();
        let first_workspace = app.project.workspaces[0].id;
        let first_chat = app.project.workspaces[0].chats[0].id;

        assert_eq!(app.nav_len(), 5);
        assert_eq!(
            app.nav_items().first().copied(),
            Some(NavItem::Chat {
                workspace: first_workspace,
                chat: first_chat,
            })
        );
        assert_eq!(app.selected_item(), app.nav_items().first().copied());
    }

    #[test]
    fn select_next_previous_wrap_around_the_nav_list() {
        let mut app = App::two_workspaces();
        let items = app.nav_items();
        assert!(
            items.len() >= 2,
            "default seed should have multiple nav items"
        );

        app.select_item(items[0]);
        app.select_previous();
        assert_eq!(app.selected_item(), items.last().copied());
        app.select_next();
        assert_eq!(app.selected_item(), Some(items[0]));
    }

    #[test]
    fn empty_selection_is_not_a_terminal_input_target() {
        let mut app = App::two_workspaces();
        app.project.workspaces.clear();
        app.reconcile_selection(None);

        assert!(!app.begin_terminal_input());
        assert_eq!(app.pty_input_target(), None);
    }

    /// F5: which pane is focused is the selection, not a second field that can
    /// disagree with it. Focusing a chat and then selecting a terminal cannot
    /// leave a `Chat` focus behind, because there is nowhere to write one.
    #[test]
    fn pane_focus_follows_the_selection_and_cannot_name_the_wrong_kind() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;

        app.select_item(NavItem::Chat { workspace, chat });
        assert!(app.begin_chat_agent_input());
        assert_eq!(app.active_focus(), Some(FocusMode::Chat));

        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        assert_eq!(app.active_focus(), Some(FocusMode::Terminal));

        // The chat-input request is refused outright when a terminal is
        // selected, so the pair can never be built by hand either.
        assert!(!app.begin_chat_agent_input());
        assert_eq!(app.active_focus(), Some(FocusMode::Terminal));
    }
}
