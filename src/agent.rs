use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use crate::model::{ChatId, ChatStatus, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentTarget {
    pub workspace: WorkspaceId,
    pub chat: ChatId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPrompt {
    pub target: AgentTarget,
    pub prompt: String,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentMessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    MessageDelta {
        target: AgentTarget,
        role: AgentMessageRole,
        text: String,
    },
    ToolCall {
        target: AgentTarget,
        name: String,
        arguments: String,
    },
    FileChanged {
        target: AgentTarget,
        path: PathBuf,
    },
    CommandStarted {
        target: AgentTarget,
        command: String,
    },
    StatusChanged {
        target: AgentTarget,
        status: ChatStatus,
    },
    Error {
        target: AgentTarget,
        message: String,
    },
}

pub trait AgentBackend {
    fn send_prompt(&mut self, prompt: AgentPrompt) -> io::Result<()>;
    fn drain_events(&mut self) -> Vec<AgentEvent>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessAgentCommand {
    pub program: String,
    pub args: Vec<String>,
}

pub struct ProcessAgentBackend {
    command: ProcessAgentCommand,
    running: BTreeMap<AgentTarget, RunningAgentProcess>,
    sender: Sender<AgentEvent>,
    receiver: Receiver<AgentEvent>,
}

struct RunningAgentProcess {
    child: Child,
}

#[derive(Debug, Default)]
pub struct NoopAgentBackend;

impl AgentBackend for NoopAgentBackend {
    fn send_prompt(&mut self, _prompt: AgentPrompt) -> io::Result<()> {
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<AgentEvent> {
        Vec::new()
    }
}

impl ProcessAgentCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    pub fn with_args(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
        }
    }

    pub fn label(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

impl ProcessAgentBackend {
    pub fn new(command: ProcessAgentCommand) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            command,
            running: BTreeMap::new(),
            sender,
            receiver,
        }
    }

    pub fn command(&self) -> &ProcessAgentCommand {
        &self.command
    }

    pub fn is_running(&self, target: AgentTarget) -> bool {
        self.running.contains_key(&target)
    }

    fn send_event(&self, event: AgentEvent) {
        let _ = self.sender.send(event);
    }
}

impl AgentBackend for ProcessAgentBackend {
    fn send_prompt(&mut self, prompt: AgentPrompt) -> io::Result<()> {
        if self.is_running(prompt.target) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "agent process is already running for this chat",
            ));
        }

        let mut command = Command::new(&self.command.program);
        command
            .args(&self.command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(cwd) = &prompt.cwd {
            command.current_dir(cwd);
        }
        command.envs(&prompt.env);

        let mut child = command.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.prompt.as_bytes())?;
            if !prompt.prompt.ends_with('\n') {
                stdin.write_all(b"\n")?;
            }
        }

        self.running
            .insert(prompt.target, RunningAgentProcess { child });
        self.send_event(AgentEvent::StatusChanged {
            target: prompt.target,
            status: ChatStatus::Thinking,
        });
        self.send_event(AgentEvent::CommandStarted {
            target: prompt.target,
            command: self.command.label(),
        });

        if let Some(stdout) = stdout {
            spawn_pipe_reader(
                prompt.target,
                AgentMessageRole::Assistant,
                stdout,
                self.sender.clone(),
            );
        }
        if let Some(stderr) = stderr {
            spawn_pipe_reader(
                prompt.target,
                AgentMessageRole::System,
                stderr,
                self.sender.clone(),
            );
        }

        Ok(())
    }

    fn drain_events(&mut self) -> Vec<AgentEvent> {
        let mut events = drain_receiver(&self.receiver);
        let mut exited = Vec::new();

        for (target, process) in &mut self.running {
            match process.child.try_wait() {
                Ok(Some(status)) => exited.push((*target, status)),
                Ok(None) => {}
                Err(error) => {
                    events.push(AgentEvent::Error {
                        target: *target,
                        message: format!("failed to poll agent process: {error}"),
                    });
                    exited.push((*target, failed_status()));
                }
            }
        }

        for (target, status) in exited {
            self.running.remove(&target);
            if status.success() {
                events.push(AgentEvent::StatusChanged {
                    target,
                    status: ChatStatus::Done,
                });
            } else {
                events.push(AgentEvent::Error {
                    target,
                    message: format!("agent process exited with {status}"),
                });
            }
        }

        events.extend(drain_receiver(&self.receiver));
        events
    }
}

impl Drop for ProcessAgentBackend {
    fn drop(&mut self) {
        for (_, mut process) in std::mem::take(&mut self.running) {
            let _ = process.child.kill();
        }
    }
}

impl AgentEvent {
    pub fn target(&self) -> AgentTarget {
        match self {
            Self::MessageDelta { target, .. }
            | Self::ToolCall { target, .. }
            | Self::FileChanged { target, .. }
            | Self::CommandStarted { target, .. }
            | Self::StatusChanged { target, .. }
            | Self::Error { target, .. } => *target,
        }
    }
}

impl AgentMessageRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "agent",
            Self::System => "system",
            Self::Tool => "tool",
        }
    }
}

fn drain_receiver(receiver: &Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

fn spawn_pipe_reader(
    target: AgentTarget,
    role: AgentMessageRole,
    mut reader: impl Read + Send + 'static,
    sender: Sender<AgentEvent>,
) {
    thread::spawn(move || {
        let mut buffer = [0; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                    if sender
                        .send(AgentEvent::MessageDelta { target, role, text })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let _ = sender.send(AgentEvent::Error {
                        target,
                        message: format!("failed to read agent output: {error}"),
                    });
                    break;
                }
            }
        }
    });
}

#[cfg(unix)]
fn failed_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    ExitStatus::from_raw(1 << 8)
}

#[cfg(windows)]
fn failed_status() -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;

    ExitStatus::from_raw(1)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn agent_event_exposes_target() {
        let target = AgentTarget {
            workspace: WorkspaceId(1),
            chat: ChatId(2),
        };
        let event = AgentEvent::StatusChanged {
            target,
            status: ChatStatus::Thinking,
        };

        assert_eq!(event.target(), target);
    }

    #[test]
    fn noop_backend_accepts_prompts_and_has_no_events() {
        let mut backend = NoopAgentBackend;
        let prompt = AgentPrompt {
            target: AgentTarget {
                workspace: WorkspaceId(1),
                chat: ChatId(2),
            },
            prompt: "hello".to_string(),
            cwd: None,
            env: BTreeMap::new(),
        };

        backend.send_prompt(prompt).expect("send prompt");

        assert!(backend.drain_events().is_empty());
    }

    #[test]
    fn process_command_has_human_label() {
        let command = ProcessAgentCommand::with_args(
            "agent-cli",
            ["--model".to_string(), "local".to_string()],
        );

        assert_eq!(command.label(), "agent-cli --model local");
    }

    #[cfg(unix)]
    #[test]
    fn process_backend_streams_stdout_and_completion() {
        let target = AgentTarget {
            workspace: WorkspaceId(1),
            chat: ChatId(2),
        };
        let mut backend = ProcessAgentBackend::new(ProcessAgentCommand::new("cat"));

        backend
            .send_prompt(AgentPrompt {
                target,
                prompt: "hello from prompt".to_string(),
                cwd: None,
                env: BTreeMap::new(),
            })
            .expect("send prompt to process");

        let events = collect_events_until(&mut backend, Duration::from_secs(2), |events| {
            events.iter().any(|event| {
                matches!(
                    event,
                    AgentEvent::StatusChanged {
                        status: ChatStatus::Done,
                        ..
                    }
                )
            }) && events.iter().any(|event| {
                matches!(
                    event,
                    AgentEvent::MessageDelta {
                        role: AgentMessageRole::Assistant,
                        text,
                        ..
                    } if text.contains("hello from prompt")
                )
            })
        });

        assert!(events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::StatusChanged {
                    target: event_target,
                    status: ChatStatus::Thinking,
                } if *event_target == target
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::CommandStarted {
                    target: event_target,
                    command,
                } if *event_target == target && command == "cat"
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::MessageDelta {
                    target: event_target,
                    role: AgentMessageRole::Assistant,
                    text,
                } if *event_target == target && text.contains("hello from prompt")
            )
        }));
        assert!(!backend.is_running(target));
    }

    fn collect_events_until(
        backend: &mut impl AgentBackend,
        timeout: Duration,
        mut done: impl FnMut(&[AgentEvent]) -> bool,
    ) -> Vec<AgentEvent> {
        let start = Instant::now();
        let mut events = Vec::new();
        while start.elapsed() < timeout {
            events.extend(backend.drain_events());
            if done(&events) {
                return events;
            }
            thread::sleep(Duration::from_millis(10));
        }
        events
    }
}
