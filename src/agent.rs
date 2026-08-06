use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};

use crate::model::{ChatId, ChatStatus, WorkspaceId};

const AGENT_EVENT_QUEUE_CAPACITY: usize = 1_024;

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
    sender: SyncSender<AgentEvent>,
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
        let (sender, receiver) = mpsc::sync_channel(AGENT_EVENT_QUEUE_CAPACITY);
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
        let stdin = child.stdin.take();

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

        // Spawn the output readers before sending the prompt so the child can
        // never block writing stdout while we block writing stdin. The prompt is
        // written from its own thread as well, so a prompt larger than the OS
        // pipe buffer cannot stall the UI even if the child is slow to drain it.
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
        if let Some(stdin) = stdin {
            spawn_prompt_writer(prompt.target, prompt.prompt, stdin, self.sender.clone());
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
    sender: SyncSender<AgentEvent>,
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

fn spawn_prompt_writer(
    target: AgentTarget,
    prompt: String,
    mut stdin: ChildStdin,
    sender: SyncSender<AgentEvent>,
) {
    thread::spawn(move || {
        let result = stdin
            .write_all(prompt.as_bytes())
            .and_then(|()| {
                if prompt.ends_with('\n') {
                    Ok(())
                } else {
                    stdin.write_all(b"\n")
                }
            })
            .and_then(|()| stdin.flush());
        // Dropping `stdin` here closes the pipe so the child sees EOF on its
        // input. A broken pipe just means the child exited or stopped reading,
        // which the process-exit path already reports; surface anything else.
        if let Err(error) = result {
            if error.kind() != io::ErrorKind::BrokenPipe {
                let _ = sender.send(AgentEvent::Error {
                    target,
                    message: format!("failed to send prompt to agent process: {error}"),
                });
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

/// # Status: scaffolding, not yet wired
///
/// Nothing in production constructs a `dyn AgentBackend` and `send_prompt` has
/// no production caller — chats currently reach their agent through the PTY
/// path, not through this module. It is deliberate in-progress scaffolding for
/// the owner's Phase 3.4, so these tests are about making the module *safe to
/// wire up later* rather than about live behaviour: every one of them drives
/// `ProcessAgentBackend` (or its private thread helpers) directly.
///
/// They deliberately use only `cat`, which the Nix build sandbox provides;
/// nothing here may depend on `$SHELL`.
#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// G11: generous, because it is only ever reached when something is wrong.
    /// `collect_events_until` returns the instant its predicate holds, so the
    /// happy path costs one poll interval, not this.
    const AGENT_TEST_TIMEOUT: Duration = Duration::from_secs(20);
    const AGENT_TEST_POLL: Duration = Duration::from_millis(5);

    fn target(chat: u64) -> AgentTarget {
        AgentTarget {
            workspace: WorkspaceId(1),
            chat: ChatId(chat),
        }
    }

    fn prompt_for(target: AgentTarget) -> AgentPrompt {
        AgentPrompt {
            target,
            prompt: "hello".to_string(),
            cwd: None,
            env: BTreeMap::new(),
        }
    }

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
        let target = target(2);
        let mut backend = ProcessAgentBackend::new(ProcessAgentCommand::new("cat"));

        backend
            .send_prompt(AgentPrompt {
                target,
                prompt: "hello from prompt".to_string(),
                cwd: None,
                env: BTreeMap::new(),
            })
            .expect("send prompt to process");

        let events = collect_events_until(&mut backend, AGENT_TEST_TIMEOUT, |events| {
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

    /// G10: a spawn that never happens must not leave the chat marked running.
    #[cfg(unix)]
    #[test]
    fn a_spawn_failure_leaves_no_running_process_and_reports_the_error() {
        let target = target(3);
        let mut backend = ProcessAgentBackend::new(ProcessAgentCommand::new(
            "mult-agent-binary-that-does-not-exist",
        ));

        let error = backend
            .send_prompt(prompt_for(target))
            .expect_err("a missing program cannot be spawned");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            !backend.is_running(target),
            "a process that never started must not be tracked as running"
        );
        // The failure is reported through the return value only: `send_prompt`
        // fails before the Thinking/CommandStarted pair is queued, so nothing
        // arrives on the event stream and nothing has to be retracted. Whatever
        // wires this up (Phase 3.4) must surface the `Err` itself — watching
        // events alone would show the chat neither starting nor failing.
        assert!(backend.drain_events().is_empty());
    }

    /// G10: a nonzero exit is a failure, not a completion.
    #[cfg(unix)]
    #[test]
    fn a_nonzero_exit_is_reported_as_an_error_rather_than_done() {
        let target = target(4);
        // `cat` of a path that cannot exist exits 1 without needing a shell.
        let mut backend = ProcessAgentBackend::new(ProcessAgentCommand::with_args(
            "cat",
            ["/nonexistent/mult-agent-test-path".to_string()],
        ));

        backend
            .send_prompt(prompt_for(target))
            .expect("spawn the failing process");

        let events = collect_events_until(&mut backend, AGENT_TEST_TIMEOUT, |events| {
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::Error { .. }))
        });

        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentEvent::Error { target: reported, message }
                    if *reported == target && message.contains("exited with")
            )),
            "a nonzero exit must be an Error naming the status: {events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AgentEvent::StatusChanged {
                    status: ChatStatus::Done,
                    ..
                }
            )),
            "a failed run must never also report Done: {events:?}"
        );
        assert!(!backend.is_running(target));
    }

    /// G10: agent output is bytes, not text. A half-written multi-byte sequence
    /// at the 8 KiB read boundary is routine, and a binary blob is possible.
    #[test]
    fn the_pipe_reader_survives_invalid_utf8() {
        let target = target(5);
        let (sender, receiver) = mpsc::sync_channel(AGENT_EVENT_QUEUE_CAPACITY);
        let mut bytes = vec![0x80, 0xff, 0xfe];
        bytes.extend_from_slice(b"still reading");

        spawn_pipe_reader(
            target,
            AgentMessageRole::Assistant,
            io::Cursor::new(bytes),
            sender,
        );

        let mut text = String::new();
        let deadline = Instant::now() + AGENT_TEST_TIMEOUT;
        while Instant::now() < deadline && !text.contains("still reading") {
            match receiver.recv_timeout(AGENT_TEST_POLL) {
                Ok(AgentEvent::MessageDelta { text: delta, .. }) => text.push_str(&delta),
                Ok(other) => panic!("unexpected event from the pipe reader: {other:?}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        assert!(
            text.contains("still reading"),
            "the reader must keep going past invalid bytes: {text:?}"
        );
        assert!(
            text.contains('\u{fffd}'),
            "invalid bytes must be replaced, not dropped: {text:?}"
        );
    }

    /// G10: the queue is bounded at 1024 events, so a chatty agent *will* fill
    /// it. Backpressure is the intended design — what must not happen is a lost
    /// byte or a reader that never finishes.
    #[test]
    fn queue_backpressure_delays_a_pipe_reader_without_losing_its_bytes() {
        const READS: usize = AGENT_EVENT_QUEUE_CAPACITY * 3;

        let target = target(6);
        let (sender, receiver) = mpsc::sync_channel(AGENT_EVENT_QUEUE_CAPACITY);
        // One byte per read means one event per read, so the reader blocks on
        // `send` after 1024 of the 3072 reads and can only continue as the
        // consumer drains.
        spawn_pipe_reader(
            target,
            AgentMessageRole::Assistant,
            OneBytePerRead { remaining: READS },
            sender,
        );

        let mut received = 0;
        let deadline = Instant::now() + AGENT_TEST_TIMEOUT;
        while received < READS && Instant::now() < deadline {
            match receiver.recv_timeout(AGENT_TEST_POLL) {
                Ok(AgentEvent::MessageDelta { text, .. }) => received += text.len(),
                Ok(other) => panic!("unexpected event from the pipe reader: {other:?}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        assert_eq!(
            received, READS,
            "backpressure may delay a pipe reader but must never drop its bytes"
        );
    }

    /// G10: the other half of backpressure. A blocked reader whose consumer has
    /// gone away must fail its `send` and exit, not pin a thread forever.
    #[test]
    fn a_dropped_consumer_releases_a_blocked_pipe_reader() {
        let (alive, reader_finished) = mpsc::channel::<()>();
        let (sender, receiver) = mpsc::sync_channel(4);
        spawn_pipe_reader(
            target(7),
            AgentMessageRole::Assistant,
            EndlessReader { _alive: alive },
            sender,
        );

        // Four sends fill the queue and the fifth blocks; dropping the receiver
        // must make it fail rather than wait.
        drop(receiver);

        assert!(
            matches!(
                reader_finished.recv_timeout(AGENT_TEST_TIMEOUT),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ),
            "the reader thread must exit once nobody is listening"
        );
    }

    /// Yields exactly one byte per `read`, then EOF.
    struct OneBytePerRead {
        remaining: usize,
    }

    impl Read for OneBytePerRead {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 || output.is_empty() {
                return Ok(0);
            }
            self.remaining -= 1;
            output[0] = b'x';
            Ok(1)
        }
    }

    /// Never reaches EOF. Its `_alive` sender is dropped with the reader when
    /// the reader thread ends, which is how the test observes that exit.
    struct EndlessReader {
        _alive: mpsc::Sender<()>,
    }

    impl Read for EndlessReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if output.is_empty() {
                return Ok(0);
            }
            output[0] = b'x';
            Ok(1)
        }
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
            thread::sleep(AGENT_TEST_POLL);
        }
        events
    }
}
