//! Filtering a pane's lines by a query.
//!
//! Chats and terminals share one implementation: a chat's searchable content is
//! its agent PTY's screen — the same place the pane itself is rendered from
//! (F1).

use super::{App, NavItem, Prompt, PromptInput, SearchPrompt};
use crate::model::{ChatId, TerminalId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub scope: SearchScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Terminal(TerminalId),
    Chat(ChatId),
}

impl App {
    pub fn begin_search(&mut self) -> bool {
        let Some(scope) = self.selected_search_scope() else {
            return false;
        };
        let input = self
            .active_search
            .as_ref()
            .filter(|search| search.scope == scope)
            .map(|search| search.query.clone())
            .unwrap_or_default();
        self.open_prompt(Prompt::Search(SearchPrompt {
            input: PromptInput::new(input),
            scope,
            error: None,
        }));
        true
    }

    pub fn submit_search(&mut self) {
        let Some(Prompt::Search(prompt)) = self.mode.prompt() else {
            return;
        };
        let query = prompt.input.trim().to_string();
        if query.is_empty() {
            self.active_search = None;
        } else {
            self.active_search = Some(SearchState {
                query,
                scope: prompt.scope,
            });
        }
        self.close_prompt();
    }

    pub fn clear_search(&mut self) {
        self.active_search = None;
    }

    pub fn selected_search_scope(&self) -> Option<SearchScope> {
        match self.selected_item()? {
            NavItem::Chat { chat, .. } => Some(SearchScope::Chat(chat)),
            NavItem::Terminal { terminal, .. } => Some(SearchScope::Terminal(terminal)),
        }
    }

    /// Filter a pane's lines by the active search, or `None` when no search
    /// targets that pane.
    ///
    /// `lines` is a closure rather than a value because that is the overwhelming
    /// common case: the renderer calls this every frame, scraping the screen is
    /// a `String` per row plus a `Vec`, and with no search active none of it is
    /// ever looked at (D3).
    ///
    /// Chats and terminals share this one implementation. A chat used to search
    /// a persisted transcript instead, which nothing ever wrote (F1), so the
    /// searchable content of a chat is its agent PTY's screen — the same place
    /// the pane itself is rendered from.
    pub fn search_matches(
        &self,
        scope: SearchScope,
        lines: impl FnOnce() -> Vec<String>,
    ) -> Option<Vec<String>> {
        let search = self.active_search.as_ref()?;
        if search.scope != scope {
            return None;
        }
        Some(filter_lines(lines(), &search.query))
    }

    /// The active search query when it targets `scope`, for the result header.
    pub fn active_search_query_for(&self, scope: SearchScope) -> Option<&str> {
        self.active_search
            .as_ref()
            .filter(|search| search.scope == scope)
            .map(|search| search.query.as_str())
    }
}

fn filter_lines(lines: Vec<String>, query: &str) -> Vec<String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return lines;
    }

    lines
        .into_iter()
        .filter(|line| line.to_lowercase().contains(&query))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_search_filters_all_scrollback_lines() {
        let mut app = App::two_workspaces();
        let workspace = app.project.workspaces[0].id;
        let terminal = app.project.workspaces[0].terminals[0].id;
        app.select_item(NavItem::Terminal {
            workspace,
            terminal,
        });
        assert!(app.begin_search());
        for ch in "alp".chars() {
            app.push_prompt_char(ch);
        }
        app.submit_search();

        let lines = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        assert_eq!(
            app.search_matches(SearchScope::Terminal(terminal), || lines.clone()),
            Some(vec!["alpha".to_string()])
        );
    }

    /// Chat search runs over the same lines the chat pane renders — its agent
    /// PTY's screen. It used to filter a persisted transcript that nothing in
    /// production ever wrote (F1), so it always matched nothing.
    #[test]
    fn chat_search_filters_the_agent_pty_lines() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        app.select_item(NavItem::Chat { workspace, chat });

        assert!(app.begin_search());
        for ch in "needle".chars() {
            app.push_prompt_char(ch);
        }
        app.submit_search();

        let lines = vec!["haystack".to_string(), "needle here".to_string()];
        assert_eq!(
            app.search_matches(SearchScope::Chat(chat), || lines.clone()),
            Some(vec!["needle here".to_string()])
        );
    }
}
