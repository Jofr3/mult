//! Search over a pane, and the chat transcript it searches.

use crate::{
    agent::{AgentEvent, AgentMessageRole, AgentTarget},
    model::{ChatId, ChatMessage, ChatMessageRole, ChatStatus, TerminalId},
};

use super::*;
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatBuffer {
    lines: Vec<String>,
    partial: String,
    partial_role: Option<ChatMessageRole>,
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
        self.set_prompt(Prompt::Search(SearchPrompt {
            input: PromptInput::new(input),
            scope,
            error: None,
        }));
        true
    }

    pub fn submit_search(&mut self) {
        let Some(Prompt::Search(prompt)) = self.prompt() else {
            return;
        };
        let query = prompt.input.as_str().trim().to_string();
        if query.is_empty() {
            self.active_search = None;
        } else {
            self.active_search = Some(SearchState {
                query,
                scope: prompt.scope,
            });
        }
        self.clear_prompt();
    }

    pub fn clear_search(&mut self) {
        self.active_search = None;
    }

    /// Note (E12): the transcript this counts against is the structured one,
    /// which only the experimental process-agent backend writes and which
    /// nothing calls today, so the count is `0` for every chat a user can
    /// create. The chat pane says so where the user can see it; this helper is
    /// kept for the backend being wired up, not removed.
    pub fn chat_search_status(&self) -> Option<String> {
        let search = self.active_search.as_ref()?;
        let SearchScope::Chat(chat) = search.scope else {
            return None;
        };
        let count = filter_lines(self.chat_transcript_lines(chat), &search.query).len();
        Some(format!(
            "search chat: {count} match{} for `{}`",
            if count == 1 { "" } else { "es" },
            search.query
        ))
    }

    /// `lines` is a closure rather than a value because the caller's only source
    /// of terminal text is a full screen scrape (a `String` per row): with no
    /// search active — the common case, every frame — this returns before the
    /// scrape ever runs.
    pub fn terminal_search_matches(
        &self,
        terminal: TerminalId,
        lines: impl FnOnce() -> Vec<String>,
    ) -> Option<Vec<String>> {
        let search = self.active_search.as_ref()?;
        if search.scope != SearchScope::Terminal(terminal) {
            return None;
        }
        Some(filter_lines(lines(), &search.query))
    }

    pub fn terminal_search_status(
        &self,
        terminal: TerminalId,
        lines: Vec<String>,
    ) -> Option<String> {
        let search = self.active_search.as_ref()?;
        if search.scope != SearchScope::Terminal(terminal) {
            return None;
        }
        let count = filter_lines(lines, &search.query).len();
        Some(format!(
            "search terminal: {count} match{} for `{}`",
            if count == 1 { "" } else { "es" },
            search.query
        ))
    }

    pub fn chat_lines(&self, chat: ChatId) -> Vec<String> {
        self.chat_buffers
            .get(&chat)
            .map(ChatBuffer::visible_lines)
            .unwrap_or_default()
    }

    pub fn chat_transcript_lines(&self, chat: ChatId) -> Vec<String> {
        let mut lines = self
            .project
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.chats.iter())
            .find(|session| session.id == chat)
            .map(|session| transcript_lines(&session.messages))
            .unwrap_or_default();
        if lines.is_empty() {
            lines = self.chat_lines(chat);
        }
        lines
    }

    pub fn filtered_chat_lines(&self, chat: ChatId) -> Option<Vec<String>> {
        let search = self.active_search.as_ref()?;
        if search.scope != SearchScope::Chat(chat) {
            return None;
        }
        Some(filter_lines(
            self.chat_transcript_lines(chat),
            &search.query,
        ))
    }

    pub fn active_search_query_for_chat(&self, chat: ChatId) -> Option<&str> {
        self.active_search
            .as_ref()
            .filter(|search| search.scope == SearchScope::Chat(chat))
            .map(|search| search.query.as_str())
    }

    fn append_chat_message(
        &mut self,
        target: AgentTarget,
        role: ChatMessageRole,
        text: impl Into<String>,
    ) {
        let text = text.into();
        self.chat_buffers
            .entry(target.chat)
            .or_default()
            .append_delta(role, &format!("{text}\n"));
        if self
            .project
            .append_chat_message(target.workspace, target.chat, role, text)
        {
            self.dirty = true;
        }
    }

    pub fn apply_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::MessageDelta {
                target, role, text, ..
            } => {
                let role = chat_role_from_agent(role);
                self.chat_buffers
                    .entry(target.chat)
                    .or_default()
                    .append_delta(role, &text);
                if self
                    .project
                    .append_chat_delta(target.workspace, target.chat, role, &text)
                {
                    self.dirty = true;
                }
            }
            AgentEvent::ToolCall {
                target,
                name,
                arguments,
            } => {
                let text = if arguments.is_empty() {
                    name
                } else {
                    format!("{name} {arguments}")
                };
                self.append_chat_message(target, ChatMessageRole::Tool, text);
            }
            AgentEvent::FileChanged { target, path } => {
                self.append_chat_message(
                    target,
                    ChatMessageRole::System,
                    format!("file changed: {}", path.display()),
                );
            }
            AgentEvent::CommandStarted { target, command } => {
                self.append_chat_message(
                    target,
                    ChatMessageRole::System,
                    format!("cmd: {command}"),
                );
            }
            AgentEvent::StatusChanged { target, status } => {
                self.mark_chat_status_by_id(target.chat, status);
            }
            AgentEvent::Error { target, message } => {
                self.mark_chat_status_by_id(target.chat, ChatStatus::Failed);
                self.append_chat_message(target, ChatMessageRole::Error, message);
            }
        }
    }
}

fn transcript_lines(messages: &[ChatMessage]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| {
            let prefix = format!("{} > ", message.role.label());
            message
                .text
                .lines()
                .map(move |line| format!("{prefix}{line}"))
                .collect::<Vec<_>>()
        })
        .collect()
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

impl ChatBuffer {
    const MAX_LINES: usize = 500;

    pub(super) fn from_messages(messages: &[ChatMessage]) -> Self {
        let mut buffer = Self::default();
        for message in messages {
            buffer.append_delta(message.role, &message.text);
            buffer.flush_partial();
        }
        buffer
    }

    pub(super) fn is_empty(&self) -> bool {
        self.lines.is_empty() && !self.partial_has_content()
    }

    fn append_delta(&mut self, role: ChatMessageRole, text: &str) {
        if self.partial_role != Some(role) {
            self.flush_partial();
            self.partial = format!("{} > ", role.label());
            self.partial_role = Some(role);
        }

        for ch in text.chars() {
            match ch {
                '\n' => {
                    self.flush_partial();
                    self.partial = format!("{} > ", role.label());
                    self.partial_role = Some(role);
                }
                '\r' => {
                    self.partial = format!("{} > ", role.label());
                    self.partial_role = Some(role);
                }
                '\t' => self.partial.push(' '),
                ch if ch.is_control() => {}
                ch => self.partial.push(ch),
            }
        }
    }

    fn visible_lines(&self) -> Vec<String> {
        let mut lines = self.lines.clone();
        if self.partial_has_content() {
            lines.push(self.partial.clone());
        }
        lines
    }

    fn flush_partial(&mut self) {
        if self.partial_has_content() {
            let line = std::mem::take(&mut self.partial);
            self.push_line(line);
        } else {
            self.partial.clear();
        }
        self.partial_role = None;
    }

    fn partial_has_content(&self) -> bool {
        self.partial_role
            .is_some_and(|role| self.partial != format!("{} > ", role.label()))
    }

    fn push_line(&mut self, line: String) {
        self.lines.push(line);
        let overflow = self.lines.len().saturating_sub(Self::MAX_LINES);
        if overflow > 0 {
            self.lines.drain(..overflow);
        }
    }
}

fn chat_role_from_agent(role: AgentMessageRole) -> ChatMessageRole {
    match role {
        AgentMessageRole::User => ChatMessageRole::User,
        AgentMessageRole::Assistant => ChatMessageRole::Assistant,
        AgentMessageRole::System => ChatMessageRole::System,
        AgentMessageRole::Tool => ChatMessageRole::Tool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_search_filters_all_scrollback_lines() {
        let mut app = App::default();
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
            app.terminal_search_matches(terminal, || lines.clone()),
            Some(vec!["alpha".to_string()])
        );
        assert!(app
            .terminal_search_status(terminal, lines)
            .unwrap()
            .contains("1 match"));
    }

    #[test]
    fn terminal_search_matches_skips_the_scrape_without_an_active_search() {
        let app = App::default();
        let terminal = app.project.workspaces[0].terminals[0].id;
        let mut scraped = false;

        let matches = app.terminal_search_matches(terminal, || {
            scraped = true;
            Vec::new()
        });

        assert_eq!(matches, None);
        assert!(
            !scraped,
            "with no search active the caller's screen scrape must never run"
        );
    }

    #[test]
    fn chat_search_filters_persisted_transcript_lines() {
        let mut state = ProjectState::seeded();
        let workspace = state.workspaces[0].id;
        let chat = state.workspaces[0].chats[0].id;
        state.append_chat_message(
            workspace,
            chat,
            ChatMessageRole::Assistant,
            "first\nneedle here".to_string(),
        );
        let mut app = App::new(state);
        app.select_item(NavItem::Chat { workspace, chat });

        assert!(app.begin_search());
        for ch in "needle".chars() {
            app.push_prompt_char(ch);
        }
        app.submit_search();

        assert_eq!(
            app.filtered_chat_lines(chat),
            Some(vec!["agent > needle here".to_string()])
        );
    }

    #[test]
    fn agent_message_event_appends_chat_transcript() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        let target = crate::agent::AgentTarget { workspace, chat };

        app.apply_agent_event(crate::agent::AgentEvent::MessageDelta {
            target,
            role: crate::agent::AgentMessageRole::Assistant,
            text: "hello".to_string(),
        });
        app.apply_agent_event(crate::agent::AgentEvent::MessageDelta {
            target,
            role: crate::agent::AgentMessageRole::Assistant,
            text: " world\nnext".to_string(),
        });

        assert_eq!(
            app.chat_lines(chat),
            vec![
                "agent > hello world".to_string(),
                "agent > next".to_string()
            ]
        );
        let messages = &app.project.chat(workspace, chat).unwrap().messages;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, ChatMessageRole::Assistant);
        assert_eq!(messages[0].text, "hello world\nnext");
        assert!(app.is_dirty());
    }

    #[test]
    fn app_hydrates_chat_transcript_from_project_state() {
        let mut state = ProjectState::seeded();
        let workspace = state.workspaces[0].id;
        let chat = state.workspaces[0].chats[0].id;
        state.append_chat_message(workspace, chat, ChatMessageRole::User, "hello".to_string());
        state.append_chat_message(
            workspace,
            chat,
            ChatMessageRole::Assistant,
            "hi there".to_string(),
        );

        let app = App::new(state);

        assert_eq!(
            app.chat_lines(chat),
            vec!["user > hello".to_string(), "agent > hi there".to_string()]
        );
        assert!(!app.is_dirty());
    }

    #[test]
    fn agent_status_and_error_events_update_chat_status() {
        let mut app = App::seeded();
        let workspace = app.project.workspaces[0].id;
        let chat = app.project.workspaces[0].chats[0].id;
        let target = crate::agent::AgentTarget { workspace, chat };

        app.apply_agent_event(crate::agent::AgentEvent::StatusChanged {
            target,
            status: ChatStatus::Done,
        });

        // F16: the chat the user is already looking at counts as seen the
        // instant it finishes, so the seen bit is decided in the same
        // assignment as the status rather than in a separate table afterwards.
        assert_eq!(
            app.project.chat(workspace, chat).unwrap().status,
            ChatStatus::DoneSeen
        );
        assert!(app.chat_done_seen(chat));
        assert!(app.is_dirty());

        app.mark_clean();
        app.apply_agent_event(crate::agent::AgentEvent::Error {
            target,
            message: "backend failed".to_string(),
        });

        assert_eq!(
            app.project.chat(workspace, chat).unwrap().status,
            ChatStatus::Failed
        );
        assert_eq!(
            app.chat_lines(chat),
            vec!["error > backend failed".to_string()]
        );
        assert_eq!(
            app.project.chat(workspace, chat).unwrap().messages[0].role,
            ChatMessageRole::Error
        );
        assert!(app.is_dirty());
    }
}
