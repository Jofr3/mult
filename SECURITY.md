# Security policy

`mult` is a local-first developer tool. The client and the `mult-server`
daemon communicate over a per-user Unix domain socket (mode `0600`); project
state, configuration and runtime files are written with `0600`/`0700`
permissions. The threat model is primarily multi-user machines, hostile
repositories or state files opened by the user, and crash-safety — there is no
network listener.

## What is enforced

- **Socket peer identity.** Both ends verify that the process on the other end
  runs as the same effective UID before exchanging anything, using
  `SO_PEERCRED` on Linux/Android and `getpeereid(3)` on macOS and the BSDs. A
  peer whose credentials cannot be obtained — including on a platform with no
  such API — is **rejected**, not accepted.
- **State and configuration reads.** The containing directory must be owned by
  this user and not writable by group or others, every parent component is
  opened with `O_NOFOLLOW`, and the file itself must be a regular,
  singly-linked, owner-only file read under a size cap. This applies to
  `$MULT_STATE_PATH`/`$XDG_DATA_HOME` and to `$MULT_CONFIG_PATH`/
  `$XDG_CONFIG_HOME` alike, because the config names commands that are shell
  evaluated and auto-started. A file sitting in a directory other users can
  write is refused rather than loaded.

  The two differ in one respect. The **state** path is refused outright if any
  component is a symlink. The **config** path is resolved through symlinks
  first, and the checks above are then applied to the file it resolved *to* —
  the property enforced is that the bytes came from a file only this user can
  write, in a directory only this user can write, not that no link was
  traversed. Resolution selects a path and grants nothing; the resolved path
  contains no symlinks by construction, so the `O_NOFOLLOW` walk still refuses
  a component swapped for a link mid-read. Redirecting the link therefore gains
  an attacker nothing they did not already have: the target must still be this
  user's own owner-only, single-link file in a directory only this user can
  write, which is strictly narrower than the write access to that directory
  they needed to plant the link. This is what lets a config live in a
  dotfile repository — the layout `home-manager`, `stow` and `chezmoi` produce,
  and the only one `home-manager`'s `xdg.configFile` can produce.
- **Repositories.** Reading a workspace's branch does not execute `git`: the
  first line of `.git/HEAD` is read directly, bounded, `O_NOFOLLOW`, and
  regular-file checked, so a repository's `.git/config` (`include.path`,
  `core.fsmonitor`, `core.hooksPath`) is never parsed and the branch name is
  rejected if it contains control characters.
- **Daemon autospawn.** The client only executes a `mult-server` binary that is
  a regular file owned by this user or root, with no group/other write bit, in
  a directory with the same property; the daemon is spawned with a cleared
  environment plus an allow-list (`PATH`, `HOME`, `SHELL`, `USER`, `LOGNAME`,
  `TERM`, `LANG`, `LC_*`, `MULT_*`), so secrets exported in one client's shell
  do not become the long-lived daemon's environment — and thus every later
  pane's.

Not enforced: the contents of `TerminalLaunch::Command`, `pi_agent_command` and
`claude_code_command` are deliberately shell-evaluated (`$SHELL -lc`) when the
user starts them; protecting them means protecting the *files* they come from,
which is what the checks above do.

See [`AGENTS.md`](AGENTS.md) and [`docs/DAEMON.md`](docs/DAEMON.md) for the
security-sensitive areas (state files, runtime IPC, process spawning).

## Supported versions

This is a `0.x` prototype. Only the latest `main` is supported; fixes land on
`main` and are not backported.

## Reporting a vulnerability

Please report suspected vulnerabilities **privately** rather than opening a
public issue:

- Preferred: open a private report via GitHub Security Advisories — the
  **"Report a vulnerability"** button on the repository's **Security** tab.

Please include the affected version or commit, a description of the impact, and
ideally a reproduction. We aim to acknowledge reports within about a week and
ask for reasonable time to ship a fix before any public disclosure.
