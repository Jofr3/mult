//! The checked-out branch of a workspace that is on another machine.
//!
//! Same answer as `crate::git`, fetched a different way. A local workspace's
//! branch is a file read, cheap enough to redo every two seconds on the render
//! thread; a remote one is an `ssh` round trip, which is neither. So the probe
//! runs on its own thread, the loop reads whatever the last one returned, and a
//! workspace whose machine is unreachable simply has no branch — the same
//! answer a directory that is not a repository gives.
//!
//! What crosses the connection is `cat <path>/.git/HEAD`, not `git`. That is
//! the same refusal `crate::git` documents at length: `git -C <dir>` reads that
//! repository's config, and `include.path`, `core.fsmonitor` and
//! `core.hooksPath` are all code execution — being remote makes that worse, not
//! better, because the code would run on the machine the user keeps their work
//! on. Reading one file cannot do that, and the bytes it returns go through the
//! same validation as a local `HEAD`.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crate::{
    git,
    model::{RemoteTarget, WorkspaceId},
    remote,
};

/// How long an answer is trusted before another `ssh` is worth it.
///
/// Two seconds is right for a file read and absurd for a connection: every
/// probe is a process, a TCP handshake and a login. Thirty seconds keeps a
/// branch shown after a checkout on the other side within the same minute
/// without turning the sidebar into a traffic generator.
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// How long the probe waits to reach the machine at all.
const CONNECT_TIMEOUT_SECONDS: u32 = 5;

/// A `HEAD` file is one short line. This is `crate::git`'s cap, applied to
/// bytes arriving over a pipe from a machine this process does not trust.
const MAX_HEAD_BYTES: u64 = 4 * 1024;

type Answer = (WorkspaceId, Option<String>);

/// Branches fetched from other machines, and the probes in flight for them.
pub(super) struct RemoteBranchProbe {
    sender: mpsc::Sender<Answer>,
    answers: mpsc::Receiver<Answer>,
    known: BTreeMap<WorkspaceId, Option<String>>,
    /// When the probe now in flight (or the last one) was started, which is what
    /// [`REFRESH_INTERVAL`] is measured from.
    started: BTreeMap<WorkspaceId, Instant>,
    in_flight: BTreeSet<WorkspaceId>,
}

impl Default for RemoteBranchProbe {
    fn default() -> Self {
        let (sender, answers) = mpsc::channel();
        Self {
            sender,
            answers,
            known: BTreeMap::new(),
            started: BTreeMap::new(),
            in_flight: BTreeSet::new(),
        }
    }
}

impl RemoteBranchProbe {
    /// Take delivery of every answer that arrived since the last call.
    pub(super) fn collect_answers(&mut self) {
        while let Ok((workspace, branch)) = self.answers.try_recv() {
            self.in_flight.remove(&workspace);
            self.known.insert(workspace, branch);
        }
    }

    /// The branch last fetched for this workspace, or `None` while the first
    /// probe is still out.
    pub(super) fn branch(&self, workspace: WorkspaceId) -> Option<String> {
        self.known.get(&workspace).cloned().flatten()
    }

    /// Start a probe for this workspace unless one is already out or the last
    /// answer is still fresh.
    pub(super) fn refresh(&mut self, workspace: WorkspaceId, target: &RemoteTarget, now: Instant) {
        if self.in_flight.contains(&workspace) {
            return;
        }
        if let Some(started) = self.started.get(&workspace) {
            if now.saturating_duration_since(*started) < REFRESH_INTERVAL {
                return;
            }
        }
        let Ok(arguments) = probe_arguments(target) else {
            // A destination `ssh` could not use is already a startup warning and
            // a failure in the pane. It does not also need a thread per tick.
            self.known.insert(workspace, None);
            self.started.insert(workspace, now);
            return;
        };

        self.started.insert(workspace, now);
        self.in_flight.insert(workspace);
        let sender = self.sender.clone();
        // Detached on purpose: nothing waits for it, and the answer is delivered
        // through the channel or not at all. `ConnectTimeout` and the server
        // keepalives below bound how long one can live.
        thread::Builder::new()
            .name("mult-remote-branch".to_string())
            .spawn(move || {
                let branch = read_remote_head(&arguments)
                    .as_deref()
                    .and_then(git::branch_from_head);
                let _ = sender.send((workspace, branch));
            })
            .map_err(|_| self.in_flight.remove(&workspace))
            .ok();
    }

    /// Drop everything remembered about workspaces that no longer exist, so a
    /// long session does not accumulate one entry per closed workspace.
    pub(super) fn retain(&mut self, live: &BTreeSet<WorkspaceId>) {
        self.known.retain(|workspace, _| live.contains(workspace));
        self.started.retain(|workspace, _| live.contains(workspace));
        self.in_flight.retain(|workspace| live.contains(workspace));
    }
}

/// The `ssh` argument vector for one probe.
///
/// Spawned directly rather than through a shell, so nothing here is quoted for
/// a *local* shell — but the last argument is still handed to the remote login
/// shell by `ssh`, so the path inside it is quoted for that one.
///
/// `BatchMode=yes` is the important flag: this runs with no pane to show a
/// prompt in, so a host that would ask for a passphrase must fail immediately
/// instead of waiting for an answer that cannot arrive.
fn probe_arguments(target: &RemoteTarget) -> Result<Vec<String>, remote::RemoteError> {
    let destination = remote::check_destination(&target.host)?;
    let head = remote::head_file_token(&target.path)?;
    Ok(vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"),
        // A link that dies mid-probe would otherwise leave the thread and its
        // `ssh` waiting for a reply that is never coming.
        "-o".to_string(),
        "ServerAliveInterval=5".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=2".to_string(),
        destination.to_string(),
        format!("cat {head}"),
    ])
}

/// Run one probe and return at most [`MAX_HEAD_BYTES`] of what it printed.
///
/// The cap is enforced by reading no further, and the child is then killed
/// rather than waited on: a remote that keeps writing would otherwise fill the
/// pipe and hold this thread forever.
fn read_remote_head(arguments: &[String]) -> Option<Vec<u8>> {
    let mut child = Command::new("ssh")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut bytes = Vec::new();
    if let Some(stdout) = child.stdout.as_mut() {
        let _ = stdout.take(MAX_HEAD_BYTES + 1).read_to_end(&mut bytes);
    }
    let _ = child.kill();
    let _ = child.wait();

    (bytes.len() <= MAX_HEAD_BYTES as usize).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(host: &str, path: &str) -> RemoteTarget {
        RemoteTarget {
            host: host.to_string(),
            path: path.to_string(),
            session: "mult".to_string(),
        }
    }

    #[test]
    fn a_probe_reads_head_over_ssh_without_running_git() {
        let arguments = probe_arguments(&target("user@host", "~/projects/mult")).unwrap();

        assert_eq!(
            arguments.last().unwrap(),
            r#"cat "$HOME/projects/mult/.git/HEAD""#
        );
        assert!(
            !arguments.iter().any(|argument| argument.contains("git ")),
            "the probe reads a file; running git on the far side is code execution there"
        );
        // No pane, so no way to answer a passphrase prompt: it must fail
        // instead of hanging on one.
        assert!(arguments.iter().any(|argument| argument == "BatchMode=yes"));
        assert_eq!(arguments[arguments.len() - 2], "user@host");
    }

    #[test]
    fn a_destination_ssh_could_not_use_is_refused_before_spawning_anything() {
        assert!(probe_arguments(&target("-oProxyCommand=id", "~/x")).is_err());
    }

    /// The interval is what keeps a two-second sidebar refresh from becoming a
    /// two-second `ssh`, and an answer already in flight is never asked for
    /// twice.
    #[test]
    fn a_workspace_is_probed_at_most_once_per_interval() {
        let mut probe = RemoteBranchProbe::default();
        let workspace = WorkspaceId(1);
        let target = target("-invalid", "~/x");
        let start = Instant::now();

        // An unusable destination answers immediately and still occupies the
        // interval, so a bad config costs one attempt per interval, not one per
        // tick.
        probe.refresh(workspace, &target, start);
        let first = *probe.started.get(&workspace).unwrap();
        probe.refresh(workspace, &target, start + Duration::from_secs(1));
        assert_eq!(*probe.started.get(&workspace).unwrap(), first);

        probe.refresh(workspace, &target, start + REFRESH_INTERVAL);
        assert_ne!(*probe.started.get(&workspace).unwrap(), first);
    }

    #[test]
    fn answers_for_closed_workspaces_are_forgotten() {
        let mut probe = RemoteBranchProbe::default();
        probe.known.insert(WorkspaceId(1), Some("main".to_string()));
        probe.known.insert(WorkspaceId(2), Some("side".to_string()));
        probe.started.insert(WorkspaceId(2), Instant::now());

        probe.retain(&BTreeSet::from([WorkspaceId(1)]));

        assert_eq!(probe.branch(WorkspaceId(1)), Some("main".to_string()));
        assert_eq!(probe.branch(WorkspaceId(2)), None);
        assert!(probe.started.is_empty());
    }
}
