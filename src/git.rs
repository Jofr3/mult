//! Reading the checked-out branch of a workspace.
//!
//! This deliberately does **not** run `git`. The status bar refreshes every two
//! seconds for every open workspace, and a workspace is an arbitrary directory
//! the user pointed at — `SECURITY.md` names hostile repositories as in scope.
//! Running `git -C <cwd>` there parses that repository's `.git/config`, and
//! `include.path`, `core.fsmonitor` and `core.hooksPath` are all
//! code-execution vectors: merely *opening* a workspace would have been enough.
//!
//! So the branch is read the way the ref actually lives on disk: the first line
//! of `HEAD`. Every file involved is opened with `O_NOFOLLOW`, required to be a
//! regular file, and read under a size cap, so a hostile repository can at
//! worst supply a branch *name* — which is checked for control characters
//! before it can reach the renderer.

use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// `HEAD` and a `.git` pointer file are both a single short line. Anything
/// larger is not one, and is refused rather than buffered.
const MAX_GIT_POINTER_BYTES: usize = 4 * 1024;
/// How far up the tree a workspace may sit inside its repository. `git`
/// searches to the filesystem root; a bounded walk keeps a pathological path
/// from turning a 2 s refresh into thousands of `stat` calls.
const MAX_GIT_ANCESTOR_DEPTH: usize = 64;
/// A branch name longer than this is not being displayed in a status bar.
const MAX_BRANCH_NAME_CHARS: usize = 255;

/// The branch `HEAD` points at, or `None` for a detached `HEAD`, a directory
/// that is not inside a repository, or anything that fails validation.
pub fn current_branch(cwd: &Path) -> Option<String> {
    if !cwd.is_dir() {
        // `git -C <missing>` failed; a missing directory must not be answered
        // with an ancestor's branch.
        return None;
    }
    let git_dir = resolve_git_dir(cwd)?;
    let head = read_bounded_regular_file(&git_dir.join("HEAD"))?;
    branch_from_head(&head)
}

/// Locate the git directory governing `start`.
///
/// Walks ancestors like `git` does, but stops at the *first* `.git` entry it
/// finds: once a repository boundary exists, a broken one is an answer of
/// `None`, never a silent fall-through to the enclosing repository's branch.
fn resolve_git_dir(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors().take(MAX_GIT_ANCESTOR_DEPTH) {
        let candidate = ancestor.join(".git");
        let link_metadata = match fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            // No entry at all: this directory is not a repository root.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };

        // A symlinked `.git` is legitimate (linked worktrees, dotfile setups),
        // so it is resolved — but deliberately and exactly once, so everything
        // afterwards operates on a path whose final component is not a link. A
        // dangling link resolves to nothing and ends the search.
        let entry = if link_metadata.file_type().is_symlink() {
            fs::canonicalize(&candidate).ok()?
        } else {
            candidate
        };

        let metadata = fs::symlink_metadata(&entry).ok()?;
        if metadata.is_dir() {
            return Some(entry);
        }
        if metadata.is_file() {
            return git_dir_from_pointer(&entry, ancestor);
        }
        return None;
    }
    None
}

/// Resolve a `.git` *file* (`gitdir: <path>`), as used by linked worktrees and
/// submodules. A relative target is resolved against the directory holding the
/// pointer, exactly as git specifies.
fn git_dir_from_pointer(pointer: &Path, base: &Path) -> Option<PathBuf> {
    let contents = read_bounded_regular_file(pointer)?;
    let line = first_nonempty_line(&contents)?;
    let target = line.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }

    let target = PathBuf::from(target);
    let target = if target.is_absolute() {
        target
    } else {
        base.join(target)
    };
    target.is_dir().then_some(target)
}

/// Read a file that must be a regular file reached without traversing a
/// symlink, capped at [`MAX_GIT_POINTER_BYTES`].
fn read_bounded_regular_file(path: &Path) -> Option<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }

    let mut bytes = Vec::new();
    file.take(MAX_GIT_POINTER_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_GIT_POINTER_BYTES).then_some(bytes)
}

/// The branch named by a `HEAD` file.
///
/// `HEAD` is either `ref: refs/heads/<branch>` (on a branch) or a raw object
/// ID (detached), which is what `git symbolic-ref --quiet --short HEAD`
/// distinguished. Only `refs/heads/` is accepted: that is what checking out a
/// branch writes, and anything else is not a branch to display.
///
/// Public because the remote-branch probe reads a `HEAD` this process cannot
/// open — it arrives over a pipe from another machine — and must validate it by
/// exactly the same rules, including the control-character check below.
pub fn branch_from_head(head: &[u8]) -> Option<String> {
    let branch = first_nonempty_line(head)?
        .strip_prefix("ref:")?
        .trim()
        .strip_prefix("refs/heads/")?
        .to_string();
    is_displayable_branch(&branch).then_some(branch)
}

/// A branch name comes from a repository this process does not trust, and it is
/// rendered into the status bar, so control characters (which could rewrite the
/// line) and absurd lengths are rejected outright.
fn is_displayable_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.chars().count() <= MAX_BRANCH_NAME_CHARS
        && !branch.chars().any(char::is_control)
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
    use std::{
        os::unix::fs::symlink,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(1);

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

    #[test]
    fn current_branch_reads_checked_out_branch() {
        let root = repository("ref: refs/heads/feature/work\n");

        assert_eq!(
            current_branch(&root),
            Some("feature/work".to_string()),
            "a checked-out branch is read from .git/HEAD"
        );
    }

    #[test]
    fn current_branch_is_found_from_a_subdirectory() {
        let root = repository("ref: refs/heads/main\n");
        let nested = root.join("src").join("deep");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(current_branch(&nested), Some("main".to_string()));
    }

    #[test]
    fn detached_head_has_no_branch() {
        let root = repository("9c4b2f7a1d0e5b3c8f6a9d2e4b7c1a0f3e5d8b6c\n");

        assert_eq!(current_branch(&root), None);
    }

    #[test]
    fn non_repo_directory_has_no_branch() {
        let root = unique_test_dir();
        fs::create_dir_all(&root).unwrap();

        assert_eq!(current_branch(&root), None);
        assert_eq!(current_branch(&root.join("absent")), None);
    }

    #[test]
    fn symlinked_git_directory_is_resolved_and_a_broken_link_yields_no_branch() {
        // A workspace inside a repository, whose own `.git` is a symlink.
        let outer = repository("ref: refs/heads/outer\n");
        let inner = outer.join("linked");
        fs::create_dir_all(&inner).unwrap();
        let real_git = outer.join("elsewhere.git");
        fs::create_dir_all(&real_git).unwrap();
        fs::write(real_git.join("HEAD"), "ref: refs/heads/inner\n").unwrap();
        symlink(&real_git, inner.join(".git")).unwrap();

        assert_eq!(current_branch(&inner), Some("inner".to_string()));

        // The link now dangles. The answer must be "unknown", never the
        // enclosing repository's branch.
        fs::remove_dir_all(&real_git).unwrap();
        assert_eq!(current_branch(&inner), None);
    }

    #[test]
    fn worktree_gitdir_pointer_file_is_followed() {
        let root = repository("ref: refs/heads/main\n");
        let worktree = unique_test_dir();
        fs::create_dir_all(&worktree).unwrap();
        let worktree_git = root.join(".git").join("worktrees").join("wt");
        fs::create_dir_all(&worktree_git).unwrap();
        fs::write(worktree_git.join("HEAD"), "ref: refs/heads/side\n").unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git.display()),
        )
        .unwrap();

        assert_eq!(current_branch(&worktree), Some("side".to_string()));
    }

    #[test]
    fn a_pointer_to_nothing_yields_no_branch() {
        let root = unique_test_dir();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".git"), "gitdir: /nonexistent/mult-test\n").unwrap();

        assert_eq!(current_branch(&root), None);
    }

    #[test]
    fn a_symlinked_head_is_not_followed() {
        // `HEAD` itself is opened O_NOFOLLOW: a repository cannot aim the read
        // at an arbitrary file outside itself.
        let root = repository("ref: refs/heads/main\n");
        let secret = root.join("secret");
        fs::write(&secret, "ref: refs/heads/leaked\n").unwrap();
        let head = root.join(".git").join("HEAD");
        fs::remove_file(&head).unwrap();
        symlink(&secret, &head).unwrap();

        assert_eq!(current_branch(&root), None);
    }

    #[test]
    fn a_hostile_head_cannot_inject_control_characters() {
        let root = repository("ref: refs/heads/main\u{7}\u{1b}[2Jspoofed\n");

        assert_eq!(current_branch(&root), None);
    }

    #[test]
    fn an_oversized_head_is_refused() {
        let root = repository(&format!(
            "ref: refs/heads/{}\n",
            "x".repeat(MAX_GIT_POINTER_BYTES)
        ));

        assert_eq!(current_branch(&root), None);
    }

    /// A repository fixture built by hand: no `git` binary is required, and
    /// `.git/HEAD` is the only file the probe ever reads.
    fn repository(head: &str) -> PathBuf {
        let root = unique_test_dir();
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), head).unwrap();
        root
    }

    fn unique_test_dir() -> PathBuf {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mult-git-test-{}-{sequence}", std::process::id()))
    }
}
