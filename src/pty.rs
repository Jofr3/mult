use std::{
    collections::{BTreeMap, HashMap},
    io::{self, Read, Write},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::model::TerminalId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtySpawn {
    pub terminal: TerminalId,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub size: PtyDimensions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyDimensions {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    Output {
        terminal: TerminalId,
        text: String,
    },
    Exited {
        terminal: TerminalId,
        status: PtyExit,
    },
    Error {
        terminal: TerminalId,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyExit {
    pub code: u32,
    pub signal: Option<String>,
}

pub struct PtyRuntime {
    sessions: HashMap<TerminalId, PtySession>,
    sender: Sender<PtyEvent>,
    receiver: Receiver<PtyEvent>,
}

struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
}

impl Default for PtyRuntime {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sessions: HashMap::new(),
            sender,
            receiver,
        }
    }
}

impl PtySpawn {
    pub fn shell(
        terminal: TerminalId,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            terminal,
            program: default_shell(),
            args: Vec::new(),
            cwd,
            env,
            size: PtyDimensions::default(),
        }
    }

    pub fn command_line(
        terminal: TerminalId,
        command: String,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            terminal,
            program: default_shell(),
            args: shell_command_args(command),
            cwd,
            env,
            size: PtyDimensions::default(),
        }
    }
}

impl Default for PtyDimensions {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

impl From<PtyDimensions> for PtySize {
    fn from(size: PtyDimensions) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl PtyRuntime {
    pub fn is_running(&self, terminal: TerminalId) -> bool {
        self.sessions.contains_key(&terminal)
    }

    pub fn start(&mut self, spawn: PtySpawn) -> io::Result<()> {
        if self.is_running(spawn.terminal) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal already has a running PTY",
            ));
        }

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(spawn.size.into()).map_err(error_to_io)?;

        let mut command = CommandBuilder::new(&spawn.program);
        command.args(&spawn.args);
        if let Some(cwd) = &spawn.cwd {
            command.cwd(cwd.as_os_str());
        }
        for (key, value) in &spawn.env {
            command.env(key, value);
        }

        let child = pair.slave.spawn_command(command).map_err(error_to_io)?;
        let reader = pair.master.try_clone_reader().map_err(error_to_io)?;
        let writer = pair.master.take_writer().map_err(error_to_io)?;
        spawn_reader(spawn.terminal, reader, self.sender.clone());

        self.sessions.insert(
            spawn.terminal,
            PtySession {
                master: pair.master,
                child,
                writer,
            },
        );

        Ok(())
    }

    pub fn stop(&mut self, terminal: TerminalId) -> io::Result<bool> {
        let Some(mut session) = self.sessions.remove(&terminal) else {
            return Ok(false);
        };

        session.child.kill()?;
        Ok(true)
    }

    pub fn send_input(&mut self, terminal: TerminalId, input: &[u8]) -> io::Result<bool> {
        let Some(session) = self.sessions.get_mut(&terminal) else {
            return Ok(false);
        };

        session.writer.write_all(input)?;
        session.writer.flush()?;
        Ok(true)
    }

    pub fn resize(&mut self, terminal: TerminalId, size: PtyDimensions) -> io::Result<()> {
        let Some(session) = self.sessions.get(&terminal) else {
            return Ok(());
        };

        session.master.resize(size.into()).map_err(error_to_io)
    }

    pub fn drain_events(&mut self) -> Vec<PtyEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.receiver.try_recv() {
            events.push(event);
        }

        let mut exited = Vec::new();
        for (terminal, session) in &mut self.sessions {
            match session.child.try_wait() {
                Ok(Some(status)) => exited.push((*terminal, PtyExit::from(status))),
                Ok(None) => {}
                Err(error) => {
                    events.push(PtyEvent::Error {
                        terminal: *terminal,
                        message: format!("failed to poll child: {error}"),
                    });
                    exited.push((*terminal, PtyExit::failed()));
                }
            }
        }

        for (terminal, status) in exited {
            self.sessions.remove(&terminal);
            events.push(PtyEvent::Exited { terminal, status });
        }

        events
    }
}

impl Drop for PtyRuntime {
    fn drop(&mut self) {
        for (_, mut session) in self.sessions.drain() {
            let _ = session.child.kill();
        }
    }
}

impl From<portable_pty::ExitStatus> for PtyExit {
    fn from(status: portable_pty::ExitStatus) -> Self {
        Self {
            code: status.exit_code(),
            signal: status.signal().map(ToOwned::to_owned),
        }
    }
}

impl PtyExit {
    fn failed() -> Self {
        Self {
            code: 1,
            signal: None,
        }
    }

    pub fn label(&self) -> String {
        match &self.signal {
            Some(signal) => format!("terminated by {signal}"),
            None => format!("exit {}", self.code),
        }
    }
}

fn spawn_reader(terminal: TerminalId, mut reader: Box<dyn Read + Send>, sender: Sender<PtyEvent>) {
    thread::spawn(move || {
        let mut buffer = [0; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                    if sender.send(PtyEvent::Output { terminal, text }).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    let _ = sender.send(PtyEvent::Error {
                        terminal,
                        message: format!("failed to read PTY output: {error}"),
                    });
                    break;
                }
            }
        }
    });
}

fn shell_command_args(command: String) -> Vec<String> {
    #[cfg(windows)]
    {
        vec!["-NoExit".to_string(), "-Command".to_string(), command]
    }

    #[cfg(not(windows))]
    {
        vec!["-lc".to_string(), command]
    }
}

fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
    }

    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
}

fn error_to_io(error: anyhow::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_spawn_uses_default_size() {
        let spawn = PtySpawn::shell(TerminalId(7), None, BTreeMap::new());

        assert_eq!(spawn.terminal, TerminalId(7));
        assert_eq!(spawn.args, Vec::<String>::new());
        assert_eq!(spawn.size, PtyDimensions { rows: 24, cols: 80 });
        assert!(!spawn.program.is_empty());
    }

    #[test]
    fn pty_spawn_command_line_runs_through_shell() {
        let spawn = PtySpawn::command_line(
            TerminalId(7),
            "cargo test".to_string(),
            None,
            BTreeMap::new(),
        );

        assert_eq!(spawn.terminal, TerminalId(7));
        assert_eq!(spawn.args.last().map(String::as_str), Some("cargo test"));
        assert!(!spawn.program.is_empty());
    }

    #[test]
    fn pty_exit_has_human_label() {
        let exit = PtyExit {
            code: 2,
            signal: None,
        };

        assert_eq!(exit.label(), "exit 2");
    }
}
