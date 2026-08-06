//! Starting and focusing chat agents, plus the experimental process-agent
//! backend selected by `MULT_AGENT_CMD`.
//!
//! A persisted agent generation is restoration state, never permission to
//! launch: it is reconciled with the daemon by `Attach` only (C1).

use std::io::{self};

use mult_protocol::AGENT_STATUS_SCHEMA_VERSION;

use crate::layout::AppLayout;
use crate::{
    agent::{
        self, AgentBackend, AgentEvent, NoopAgentBackend, ProcessAgentBackend, ProcessAgentCommand,
    },
    app::App,
    config::Config,
    model::{self, AgentKind, ChatStatus, PtyKey},
    pty::{AttachExistingResult, PtyRuntime, PtySpawn},
    storage,
};

use super::agent_command::agent_command;
use super::agent_status::{
    agent_session_metadata, agent_status_kind, mult_agent_status_path,
    prepare_mult_agent_status_file, reconcile_agent_status, MULT_AGENT_CHAT_ID_ENV,
    MULT_AGENT_GENERATION_ENV, MULT_AGENT_KIND_ENV, MULT_AGENT_NAMESPACE_ENV,
    MULT_AGENT_SESSION_TOKEN_ENV, MULT_AGENT_STATUS_PATH_ENV, MULT_AGENT_STATUS_VERSION_ENV,
};
use super::save::save_if_dirty_with;
use super::session::chat_agent_dimensions;

const AGENT_CMD_ENV: &str = "MULT_AGENT_CMD";

pub(super) enum RuntimeAgentBackend {
    Noop(NoopAgentBackend),
    Process(ProcessAgentBackend),
}

impl RuntimeAgentBackend {
    pub(super) fn from_env() -> Self {
        std::env::var(AGENT_CMD_ENV)
            .ok()
            .and_then(|raw| parse_process_agent_command(&raw))
            .map(ProcessAgentBackend::new)
            .map(Self::Process)
            .unwrap_or_else(|| Self::Noop(NoopAgentBackend))
    }
}

impl AgentBackend for RuntimeAgentBackend {
    fn send_prompt(&mut self, prompt: agent::AgentPrompt) -> io::Result<()> {
        match self {
            Self::Noop(backend) => backend.send_prompt(prompt),
            Self::Process(backend) => backend.send_prompt(prompt),
        }
    }

    fn drain_events(&mut self) -> Vec<AgentEvent> {
        match self {
            Self::Noop(backend) => backend.drain_events(),
            Self::Process(backend) => backend.drain_events(),
        }
    }
}

fn parse_process_agent_command(raw: &str) -> Option<ProcessAgentCommand> {
    let mut parts = split_process_agent_command(raw).ok()?.into_iter();
    let program = parts.next()?;
    if program.is_empty() {
        return None;
    }

    Some(ProcessAgentCommand::with_args(program, parts))
}

fn split_process_agent_command(raw: &str) -> Result<Vec<String>, &'static str> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut escaping = false;
    let mut in_token = false;

    for ch in raw.chars() {
        if escaping {
            current.push(ch);
            escaping = false;
            in_token = true;
            continue;
        }

        match quote {
            Quote::None => match ch {
                '\\' => {
                    escaping = true;
                    in_token = true;
                }
                '\'' => {
                    quote = Quote::Single;
                    in_token = true;
                }
                '"' => {
                    quote = Quote::Double;
                    in_token = true;
                }
                ch if ch.is_whitespace() => {
                    if in_token {
                        args.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                }
                _ => {
                    current.push(ch);
                    in_token = true;
                }
            },
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => {
                    escaping = true;
                    in_token = true;
                }
                _ => current.push(ch),
            },
        }
    }

    if escaping {
        current.push('\\');
    }
    if quote != Quote::None {
        return Err("unterminated quote");
    }
    if in_token {
        args.push(current);
    }

    Ok(args)
}

pub(super) fn add_agent_to_selected_workspace(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    layout: AppLayout,
    agent: AgentKind,
) {
    if let Some((workspace, chat)) = app.add_chat_to_selected_workspace_and_return(agent) {
        start_or_focus_chat_agent(
            app,
            pty_runtime,
            config,
            store,
            layout,
            ChatAgentLaunch {
                workspace_id: workspace,
                chat_id: chat,
                focus_after_start: true,
            },
        );
    }
}

pub(super) fn start_or_focus_selected_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    layout: AppLayout,
) {
    let Some((workspace_id, chat_id)) = app.selected_chat_id() else {
        return;
    };

    start_or_focus_chat_agent(
        app,
        pty_runtime,
        config,
        store,
        layout,
        ChatAgentLaunch {
            workspace_id,
            chat_id,
            focus_after_start: true,
        },
    );
}

/// Which chat to start, and whether the caller wants focus moved into it once
/// it is running. Grouped so the launch site reads as one decision rather than
/// three trailing positional arguments.
#[derive(Debug, Clone, Copy)]
pub(super) struct ChatAgentLaunch {
    pub(super) workspace_id: model::WorkspaceId,
    pub(super) chat_id: model::ChatId,
    pub(super) focus_after_start: bool,
}

pub(super) fn start_or_focus_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    layout: AppLayout,
    launch: ChatAgentLaunch,
) {
    let ChatAgentLaunch {
        workspace_id,
        chat_id,
        focus_after_start,
    } = launch;
    let terminal_id = PtyKey::ChatAgent(chat_id);

    if pty_runtime.is_running(terminal_id) {
        if focus_after_start {
            app.begin_chat_agent_input();
        }
        return;
    }

    let Some(workspace) = app.project.workspace(workspace_id) else {
        return;
    };
    let (chat_name, agent, cwd, workspace_environment) = workspace
        .chats
        .iter()
        .find(|chat| chat.id == chat_id)
        .map(|chat| {
            (
                chat.name.clone(),
                chat.agent,
                workspace.cwd.clone(),
                workspace.environment.clone(),
            )
        })
        .unwrap_or_else(|| {
            (
                format!("chat {}", chat_id.0),
                AgentKind::default(),
                workspace.cwd.clone(),
                workspace.environment.clone(),
            )
        });
    let Some(identity) = app.project.session_identity(terminal_id) else {
        pty_runtime.append_terminal_system_line(terminal_id, "durable chat identity is missing");
        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
        return;
    };
    if let Err(error) = pty_runtime.register_session_identity(terminal_id, identity) {
        pty_runtime.append_terminal_system_line(
            terminal_id,
            format!("failed to register durable chat identity: {error}"),
        );
        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
        return;
    }

    // A persisted generation is restoration state, not permission to launch.
    // Reconcile it with the daemon using Attach only. If it is absent, persist
    // that fact before a later deliberate invocation may allocate a successor.
    if let Some(generation) = app.project.active_agent_generation(chat_id) {
        let metadata = agent_session_metadata(chat_id, agent, generation);
        if let Err(error) = pty_runtime.register_agent_session(terminal_id, metadata) {
            pty_runtime.append_terminal_system_line(
                terminal_id,
                format!("failed to register agent generation: {error}"),
            );
            app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
            return;
        }
        match pty_runtime.attach_existing(terminal_id, chat_agent_dimensions(layout)) {
            Ok(AttachExistingResult::Attached) => {
                reconcile_agent_status(app, pty_runtime, chat_id, agent, generation);
                if focus_after_start {
                    app.begin_chat_agent_input();
                }
                return;
            }
            Ok(AttachExistingResult::Missing) => {
                app.clear_agent_generation(chat_id, generation);
                app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
                if !persist_before_agent_launch(app, store) {
                    pty_runtime.append_terminal_system_line(
                        terminal_id,
                        "could not save missing agent generation; refusing to launch",
                    );
                    return;
                }
            }
            Err(error) => {
                app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
                pty_runtime.append_terminal_system_line(
                    terminal_id,
                    format!("failed to reconcile existing agent; refusing to relaunch: {error}"),
                );
                return;
            }
        }
    }

    let generation = match app.begin_agent_generation(chat_id) {
        Ok(Some(generation)) => generation,
        Ok(None) => return,
        Err(error) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
            pty_runtime.append_terminal_system_line(
                terminal_id,
                format!("failed to allocate secure agent generation: {error}"),
            );
            return;
        }
    };
    if !persist_before_agent_launch(app, store) {
        pty_runtime.append_terminal_system_line(
            terminal_id,
            "could not save agent generation; refusing to launch",
        );
        return;
    }

    let metadata = agent_session_metadata(chat_id, agent, generation);
    if let Err(error) = pty_runtime.register_agent_session(terminal_id, metadata) {
        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
        pty_runtime.append_terminal_system_line(
            terminal_id,
            format!("failed to register agent generation: {error}"),
        );
        return;
    }

    let command = agent_command(config, agent);
    let status_path = mult_agent_status_path(identity, generation);
    if let Err(error) = prepare_mult_agent_status_file(&status_path) {
        app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
        pty_runtime.append_terminal_system_line(
            terminal_id,
            format!("failed to prepare private agent status journal: {error}"),
        );
        return;
    }
    let mut environment = workspace_environment;
    environment.insert(
        MULT_AGENT_STATUS_PATH_ENV.to_string(),
        status_path.display().to_string(),
    );
    environment.insert(MULT_AGENT_CHAT_ID_ENV.to_string(), chat_id.0.to_string());
    environment.insert(
        MULT_AGENT_STATUS_VERSION_ENV.to_string(),
        AGENT_STATUS_SCHEMA_VERSION.to_string(),
    );
    environment.insert(
        MULT_AGENT_NAMESPACE_ENV.to_string(),
        identity.namespace.to_string(),
    );
    environment.insert(
        MULT_AGENT_SESSION_TOKEN_ENV.to_string(),
        identity.token.to_string(),
    );
    environment.insert(
        MULT_AGENT_KIND_ENV.to_string(),
        agent_status_kind(agent).to_string(),
    );
    environment.insert(
        MULT_AGENT_GENERATION_ENV.to_string(),
        generation.to_string(),
    );
    let mut spawn = PtySpawn::command_line(terminal_id, command.clone(), cwd, environment);
    spawn.size = chat_agent_dimensions(layout);

    match pty_runtime.start(spawn) {
        Ok(()) => {
            app.mark_chat_status_by_id(chat_id, ChatStatus::Idle);
            if focus_after_start {
                app.begin_chat_agent_input();
            }
        }
        Err(error) => {
            // Keep the saved generation: create delivery may be uncertain, and
            // a later attach is the only safe way to reconcile without a
            // duplicate command launch.
            app.mark_chat_status_by_id(chat_id, ChatStatus::Failed);
            pty_runtime.append_terminal_system_line(
                terminal_id,
                format!(
                    "failed to start {} agent for `{chat_name}`: {error}",
                    agent.display_name()
                ),
            );
        }
    }
}

/// An agent generation must be durable *before* the process that carries it
/// starts, so this save is forced: deferring it (B9) would risk a launched
/// agent whose generation no state file records.
fn persist_before_agent_launch(app: &mut App, store: &storage::StateStore) -> bool {
    save_if_dirty_with(app, true, |state| store.save(state));
    !app.is_dirty()
}

pub(super) fn auto_start_selected_chat_agent(
    app: &mut App,
    pty_runtime: &mut PtyRuntime,
    config: &Config,
    store: &storage::StateStore,
    layout: AppLayout,
) -> bool {
    if app.is_prompt_active() {
        return false;
    }

    let Some((workspace_id, chat_id)) = app.selected_chat_id() else {
        return false;
    };
    let agent = app
        .project
        .chat(workspace_id, chat_id)
        .map(|chat| chat.agent)
        .unwrap_or_default();
    if !auto_start_enabled(config, agent) {
        return false;
    }
    let terminal_id = PtyKey::ChatAgent(chat_id);
    if pty_runtime.is_running(terminal_id) || !pty_runtime.terminal_output_is_blank(terminal_id) {
        return false;
    }

    start_or_focus_chat_agent(
        app,
        pty_runtime,
        config,
        store,
        layout,
        ChatAgentLaunch {
            workspace_id,
            chat_id,
            focus_after_start: false,
        },
    );
    true
}

/// Whether the selected chat's agent should auto-start when its pane is
/// focused with a blank buffer. Each agent backend has its own toggle.
fn auto_start_enabled(config: &Config, agent: AgentKind) -> bool {
    match agent {
        AgentKind::Pi => config.auto_start_pi_agent,
        AgentKind::ClaudeCode => config.auto_start_claude_code_agent,
    }
}

pub(super) fn drain_agent_events(app: &mut App, backend: &mut impl AgentBackend) -> bool {
    let mut changed = false;
    for event in backend.drain_events() {
        changed = true;
        app.apply_agent_event(event);
    }
    changed
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn process_agent_command_parses_from_env_style_string() {
        let command =
            parse_process_agent_command("agent-cli --model local").expect("command parses");

        assert_eq!(command.program, "agent-cli");
        assert_eq!(command.args, vec!["--model", "local"]);
        assert_eq!(command.label(), "agent-cli --model local");
    }

    #[test]
    fn process_agent_command_supports_basic_shell_quoting() {
        let command = parse_process_agent_command(
            "agent-cli --prompt 'hello world' \"two words\" escaped\\ space",
        )
        .expect("command parses");

        assert_eq!(command.program, "agent-cli");
        assert_eq!(
            command.args,
            vec!["--prompt", "hello world", "two words", "escaped space"]
        );
    }

    #[test]
    fn blank_or_unterminated_process_agent_command_is_ignored() {
        assert_eq!(parse_process_agent_command("   "), None);
        assert_eq!(parse_process_agent_command("agent 'unterminated"), None);
    }
}
