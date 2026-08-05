use std::{
    io,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
};

use crate::model::WorkspaceId;

/// One workspace's branch probe request: its id and the directory to inspect
/// (`None` for a workspace with no working directory).
pub type BranchRequest = (WorkspaceId, Option<PathBuf>);
/// One workspace's probe result: its id and the branch name, if any.
pub type BranchResult = (WorkspaceId, Option<String>);

/// How a workspace's current branch is discovered.
///
/// The single seam between the runtime and the filesystem, so tests substitute
/// their own implementation instead of touching a repository at all.
pub trait BranchProbe: Send + 'static {
    fn current_branch(&self, cwd: &Path) -> Option<String>;
}

/// Probes the branch by reading the repository's `HEAD` file.
#[derive(Debug, Default, Clone, Copy)]
pub struct GitBranchProbe;

impl BranchProbe for GitBranchProbe {
    fn current_branch(&self, cwd: &Path) -> Option<String> {
        current_branch(cwd)
    }
}

/// How much of `HEAD` (or a `.git` gitdir pointer) is read. Both are a single
/// short line; anything larger is not a file we are interested in.
const MAX_GIT_POINTER_BYTES: u64 = 4 * 1024;

/// The branch checked out in the repository containing `cwd`, if any.
///
/// This used to run `git -C <cwd> symbolic-ref HEAD` every two seconds per
/// open workspace. That is a code-execution vector on a hostile repository:
/// `git` reads the repository's own `.git/config` before doing anything, and
/// `include.path`, `core.fsmonitor` and `core.hooksPath` all turn that config
/// into "run this". `SECURITY.md` puts hostile repositories in scope, and
/// merely *opening* a workspace was enough — the user never runs a git command.
///
/// Reading `HEAD` directly answers exactly the same question with no config
/// parsing, no hooks, no subprocess and no fork cost. Discovery still walks up
/// from `cwd` the way `git` does, and the two non-branch cases keep their
/// previous answer of `None`: a detached HEAD (a raw object id rather than a
/// `ref:` line) and a directory that is not in a repository at all.
pub fn current_branch(cwd: &Path) -> Option<String> {
    let head = read_git_pointer(&git_dir_for(cwd)?.join("HEAD")).ok()?;
    branch_from_head(&head)
}

/// The repository directory governing `cwd`: the nearest ancestor with a `.git`
/// entry, resolved through the `gitdir:` pointer file that worktrees and
/// submodules use in place of a directory.
fn git_dir_for(cwd: &Path) -> Option<PathBuf> {
    for ancestor in cwd.ancestors() {
        let dot_git = ancestor.join(".git");
        let metadata = std::fs::symlink_metadata(&dot_git).ok();
        let Some(metadata) = metadata else {
            continue;
        };
        if metadata.file_type().is_dir() {
            return Some(dot_git);
        }
        if metadata.file_type().is_file() {
            let pointer = read_git_pointer(&dot_git).ok()?;
            let target = pointer.strip_prefix("gitdir:")?.trim();
            if target.is_empty() {
                return None;
            }
            let target = PathBuf::from(target);
            return Some(if target.is_absolute() {
                target
            } else {
                ancestor.join(target)
            });
        }
    }

    None
}

/// Read one short pointer file, bounded and regular-files-only.
///
/// The contents are untrusted (this is a repository the user merely opened), so
/// the read is capped and a FIFO or device swapped in for `HEAD` is refused
/// rather than allowed to stall the probe thread. Ownership is deliberately
/// *not* required: a checkout legitimately belongs to another user or carries a
/// group-writable mode, and unlike the config or state file this content is
/// only ever displayed, never executed.
fn read_git_pointer(path: &Path) -> io::Result<String> {
    use std::io::Read;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }

    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "git pointer is not a regular file",
        ));
    }

    let mut contents = String::new();
    file.take(MAX_GIT_POINTER_BYTES)
        .read_to_string(&mut contents)?;
    Ok(contents)
}

/// The branch name from a `HEAD` file's contents.
///
/// `ref: refs/heads/<name>` is a branch. A raw object id is a detached HEAD and
/// has no branch, and so does anything else this does not recognise. The name
/// is a hostile repository's choice of bytes and ends up in the sidebar, so
/// control characters are dropped before it can reach the host terminal.
fn branch_from_head(head: &str) -> Option<String> {
    let reference = first_nonempty_line(head.as_bytes())?;
    let target = reference.strip_prefix("ref:")?.trim();
    let branch = target
        .strip_prefix("refs/heads/")?
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    (!branch.is_empty()).then_some(branch)
}

/// Runs branch probes on a worker thread.
///
/// Forking `git` costs milliseconds per workspace — more on a network
/// filesystem — and the refresh runs every couple of seconds, so doing it
/// inline made the render loop stutter for tens of milliseconds at a time. The
/// UI thread now only posts a request and picks up whatever has arrived.
pub struct BranchWatcher {
    requests: Option<Sender<Vec<BranchRequest>>>,
    results: Receiver<Vec<BranchResult>>,
    /// A request is outstanding. Requests are not queued up behind a slow
    /// probe: a refresh while one is in flight is simply skipped, since the
    /// next one carries the same (or fresher) information anyway.
    in_flight: bool,
}

impl BranchWatcher {
    pub fn new(probe: impl BranchProbe) -> Self {
        let (request_sender, request_receiver) = mpsc::channel::<Vec<BranchRequest>>();
        let (result_sender, result_receiver) = mpsc::channel::<Vec<BranchResult>>();

        thread::spawn(move || {
            // Ends when the watcher is dropped and the request sender closes.
            for request in request_receiver {
                let results = request
                    .into_iter()
                    .map(|(workspace, cwd)| {
                        let branch = cwd.as_deref().and_then(|cwd| probe.current_branch(cwd));
                        (workspace, branch)
                    })
                    .collect::<Vec<_>>();
                if result_sender.send(results).is_err() {
                    break;
                }
            }
        });

        Self {
            requests: Some(request_sender),
            results: result_receiver,
            in_flight: false,
        }
    }

    /// Ask for a refresh unless one is already outstanding. Returns whether the
    /// request was sent.
    pub fn request(&mut self, workspaces: Vec<BranchRequest>) -> bool {
        if self.in_flight || workspaces.is_empty() {
            return false;
        }
        let Some(requests) = &self.requests else {
            return false;
        };
        if requests.send(workspaces).is_err() {
            // The worker is gone (it can only end by panicking); stop asking.
            self.requests = None;
            return false;
        }
        self.in_flight = true;
        true
    }

    /// Take the results of a finished probe, if one has landed. Never blocks.
    pub fn poll(&mut self) -> Option<Vec<BranchResult>> {
        match self.results.try_recv() {
            Ok(results) => {
                self.in_flight = false;
                Some(results)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.in_flight = false;
                None
            }
        }
    }

    /// Block until the outstanding request completes. Test-only: the runtime
    /// loop must never wait on the probe thread.
    #[cfg(test)]
    fn wait(&mut self) -> Option<Vec<BranchResult>> {
        let results = self.results.recv().ok();
        self.in_flight = false;
        results
    }
}

fn first_nonempty_line(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_nonempty_line_trims_git_output() {
        assert_eq!(
            first_nonempty_line(b"\n  feature/work  \n"),
            Some("feature/work".to_string())
        );
    }

    #[test]
    fn first_nonempty_line_ignores_blank_output() {
        assert_eq!(first_nonempty_line(b"\n  \t\n"), None);
    }

    /// A repository built by hand: a `.git` directory with a `HEAD`. Nothing
    /// here shells out, so the tests do not depend on a `git` binary existing
    /// (and cannot be influenced by the machine's git configuration).
    fn temp_repo(label: &str, head: Option<&str>) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "mult-git-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("src")).expect("create worktree");
        if let Some(head) = head {
            std::fs::create_dir_all(root.join(".git")).expect("create .git");
            std::fs::write(root.join(".git").join("HEAD"), head).expect("write HEAD");
        }
        root
    }

    #[test]
    fn current_branch_reads_checked_out_branch() {
        let repo = temp_repo("branch", Some("ref: refs/heads/feature/work\n"));

        // Also proves discovery still walks up from a subdirectory, the way the
        // `git -C` invocation used to.
        assert_eq!(
            current_branch(&repo),
            Some("feature/work".to_string()),
            "the repository root reports its branch"
        );
        assert_eq!(
            current_branch(&repo.join("src")),
            Some("feature/work".to_string()),
            "a subdirectory resolves to the same repository"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn detached_head_has_no_branch() {
        let repo = temp_repo(
            "detached",
            Some("9fceb02d0ae598e95dc970b74767f19372d61af8\n"),
        );

        assert_eq!(current_branch(&repo), None);

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn non_repo_directory_has_no_branch() {
        let plain = temp_repo("non-repo", None);

        assert_eq!(current_branch(&plain), None);
        assert_eq!(current_branch(&plain.join("src")), None);

        let _ = std::fs::remove_dir_all(&plain);
    }

    #[test]
    fn a_gitdir_pointer_file_is_followed_to_the_real_repository() {
        // Worktrees and submodules replace `.git` with a `gitdir:` pointer.
        let repo = temp_repo("worktree", Some("ref: refs/heads/main\n"));
        let worktree = repo.join("linked");
        std::fs::create_dir_all(&worktree).expect("create linked worktree");
        std::fs::write(worktree.join(".git"), "gitdir: ../.git\n").expect("write pointer");

        assert_eq!(current_branch(&worktree), Some("main".to_string()));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_branch_name_cannot_carry_control_bytes_into_the_sidebar() {
        // The repository is untrusted and the branch is rendered; escapes in a
        // ref name must not reach the host terminal.
        assert_eq!(
            branch_from_head("ref: refs/heads/ma\x1b[2Jin\n"),
            Some("ma[2Jin".to_string())
        );
        assert_eq!(branch_from_head("ref: refs/heads/\n"), None);
        assert_eq!(branch_from_head("ref: refs/tags/v1\n"), None);
        assert_eq!(branch_from_head(""), None);
    }

    struct FixedProbe(&'static str);

    impl BranchProbe for FixedProbe {
        fn current_branch(&self, cwd: &Path) -> Option<String> {
            cwd.to_str()
                .filter(|cwd| cwd.contains("with-branch"))
                .map(|_| self.0.to_string())
        }
    }

    #[test]
    fn watcher_probes_off_thread_and_reports_per_workspace_results() {
        let mut watcher = BranchWatcher::new(FixedProbe("main"));

        assert!(watcher.request(vec![
            (WorkspaceId(1), Some(PathBuf::from("/tmp/with-branch"))),
            (WorkspaceId(2), Some(PathBuf::from("/tmp/plain"))),
            (WorkspaceId(3), None),
        ]));

        assert_eq!(
            watcher.wait().expect("probe results"),
            vec![
                (WorkspaceId(1), Some("main".to_string())),
                (WorkspaceId(2), None),
                (WorkspaceId(3), None),
            ]
        );
    }

    #[test]
    fn watcher_does_not_queue_a_second_request_while_one_is_in_flight() {
        let mut watcher = BranchWatcher::new(FixedProbe("main"));
        let request = vec![(WorkspaceId(1), Some(PathBuf::from("/tmp/with-branch")))];

        assert!(watcher.request(request.clone()));
        assert!(!watcher.request(request.clone()));

        watcher.wait().expect("probe results");

        // Once the results are taken the next refresh goes through again.
        assert!(watcher.request(request));
    }

    #[test]
    fn polling_an_idle_watcher_yields_nothing() {
        let mut watcher = BranchWatcher::new(FixedProbe("main"));

        assert!(watcher.poll().is_none());
    }
}
