//! The sidebar walk and the selection over it.
//!
//! One walk produces both the render order and the navigation order (F14), and
//! the selection is stored as an identity rather than an index so it can never
//! be an out-of-range position.

use crate::model::{ChatId, PtyKey, TerminalId, WorkspaceId};

use super::*;
/// One row of the sidebar, in render order. Only [`SidebarRow::Nav`] rows are
/// selectable; the others exist to be drawn and to occupy an index, which is
/// exactly what used to have to be re-derived by hand in `ui` (F14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarRow {
    /// The blank line separating two workspace groups.
    Spacer,
    /// A workspace's header line.
    Workspace(WorkspaceId),
    /// A selectable chat or terminal.
    Nav(NavItem),
}

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

impl App {
    pub fn terminal_input_target(&self) -> Option<TerminalId> {
        self.selected_terminal_id().map(|(_, terminal)| terminal)
    }

    /// The PTY the selected nav item reads and writes: a chat's agent pane or
    /// a terminal's pane.
    pub fn pty_input_target(&self) -> Option<PtyKey> {
        match self.selected_item()? {
            NavItem::Chat { chat, .. } => Some(PtyKey::ChatAgent(chat)),
            NavItem::Terminal { terminal, .. } => Some(PtyKey::Terminal(terminal)),
        }
    }

    /// The pane that currently holds the keyboard, for the purposes of the
    /// focus reports a program can ask for (DECSET 1004).
    ///
    /// This is narrower than [`Self::pty_input_target`] on purpose. A pane is
    /// focused only when the host window is focused *and* nothing modal is in
    /// front of it: while the command palette or the help overlay is up, every
    /// key belongs to that surface, so telling a program it still has the
    /// keyboard would be a lie it acts on — an editor keeps its cursor
    /// blinking, an agent keeps polling for input that cannot arrive.
    pub fn focused_pty(&self) -> Option<PtyKey> {
        if !self.host_focused || self.is_prompt_active() || self.is_help_visible() {
            return None;
        }
        self.pty_input_target()
    }

    /// Record whether the host terminal window has focus, returning whether
    /// that changed.
    pub fn set_host_focused(&mut self, focused: bool) -> bool {
        let changed = self.host_focused != focused;
        self.host_focused = focused;
        changed
    }

    /// The single source of truth for the sidebar: a blank row between
    /// workspaces, then each workspace's header, its chats and its terminals.
    ///
    /// Both the render order and the navigation order come from this one walk
    /// (F14). `ui` renders the rows it yields and finds the highlight by
    /// position in them; every nav query
    /// (`nav_items`/`nav_len`/`nav_item_at`/`nav_item_position`) is the same
    /// walk with the non-selectable rows filtered out. Nothing re-derives
    /// either order by hand, so a change here cannot move the highlight off
    /// the row it belongs to without a compile error.
    fn sidebar_row_iter(&self) -> impl Iterator<Item = SidebarRow> + '_ {
        self.project
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(index, workspace)| {
                let spacer = (index > 0).then_some(SidebarRow::Spacer);
                let header = std::iter::once(SidebarRow::Workspace(workspace.id));
                let chats = workspace.chats.iter().map(move |chat| {
                    SidebarRow::Nav(NavItem::Chat {
                        workspace: workspace.id,
                        chat: chat.id,
                    })
                });
                let terminals = workspace.terminals.iter().map(move |terminal| {
                    SidebarRow::Nav(NavItem::Terminal {
                        workspace: workspace.id,
                        terminal: terminal.id,
                    })
                });
                spacer
                    .into_iter()
                    .chain(header)
                    .chain(chats)
                    .chain(terminals)
            })
    }

    /// The sidebar's rows in render order. Collected rather than lazy because
    /// the renderer needs both the rows and the highlight's index into them.
    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        self.sidebar_row_iter().collect()
    }

    /// The selectable subset of [`Self::sidebar_row_iter`], in the same order:
    /// each workspace's chats followed by its terminals, across all workspaces.
    fn nav_iter(&self) -> impl Iterator<Item = NavItem> + '_ {
        self.sidebar_row_iter().filter_map(|row| match row {
            SidebarRow::Nav(item) => Some(item),
            SidebarRow::Spacer | SidebarRow::Workspace(_) => None,
        })
    }

    pub fn nav_items(&self) -> Vec<NavItem> {
        self.nav_iter().collect()
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

    pub fn selected_search_scope(&self) -> Option<SearchScope> {
        match self.selected_item()? {
            NavItem::Chat { chat, .. } => Some(SearchScope::Chat(chat)),
            NavItem::Terminal { terminal, .. } => Some(SearchScope::Terminal(terminal)),
        }
    }

    pub fn selected_item_can_be_deleted(&self) -> bool {
        self.selected_delete_target().is_some()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_is_focused_only_when_this_window_is_and_nothing_is_in_front_of_it() {
        let mut app = App::default();
        let selected = app.pty_input_target().expect("seed state selects a pane");
        assert_eq!(app.focused_pty(), Some(selected));

        // A modal surface owns the keyboard, so the pane behind it does not
        // have it — telling its program otherwise is a claim it acts on.
        app.begin_command_palette();
        assert_eq!(app.focused_pty(), None);
        app.cancel_prompt();
        assert_eq!(app.focused_pty(), Some(selected));

        app.show_help();
        assert_eq!(app.focused_pty(), None);
        app.hide_help();
        assert_eq!(app.focused_pty(), Some(selected));

        // ...and neither does it when the window itself has lost focus.
        assert!(app.set_host_focused(false));
        assert_eq!(app.focused_pty(), None);
        assert!(
            !app.set_host_focused(false),
            "an unchanged state is not news"
        );
        assert!(app.set_host_focused(true));
        assert_eq!(app.focused_pty(), Some(selected));

        // Selecting another pane moves the focus with it.
        app.select_next();
        let moved = app.pty_input_target().expect("another pane is selectable");
        assert_ne!(moved, selected);
        assert_eq!(app.focused_pty(), Some(moved));
    }

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
        let mut app = App::default();
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
    fn deleting_the_selected_item_selects_the_successor_in_its_slot() {
        let mut app = App::default();
        let items = app.nav_items();
        assert!(
            items.len() >= 2,
            "default seed should have multiple nav items"
        );

        app.select_item(items[0]);
        assert_eq!(app.selected_item(), Some(items[0]));

        let target = app.selected_delete_target().expect("a target is selected");
        app.delete_target(target);

        // The item that shifts into the vacated slot becomes selected (the old
        // second item), matching the position-stable behavior.
        assert_eq!(app.selected_item(), Some(items[1]));
    }

    #[test]
    fn empty_selection_is_not_a_terminal_input_target() {
        let mut app = App::default();
        app.project.workspaces.clear();
        app.reconcile_selection(None);

        assert!(!app.begin_terminal_input());
        assert_eq!(app.pty_input_target(), None);
    }
}
