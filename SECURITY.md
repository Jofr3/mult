# Security policy

`mult` is a local-first developer tool. The threat model is primarily
multi-user machines, hostile repositories or state files opened by the user,
and crash-safety — there is no network listener.

## What is enforced

**IPC.** The client and the `mult-server` daemon talk over a per-user Unix
domain socket created with mode `0600`. Both ends independently ask the kernel
for the peer's effective UID and refuse a peer that is not the same user:
`SO_PEERCRED` on Linux, `getpeereid(3)` (which is `LOCAL_PEERCRED` underneath)
on macOS and the BSDs. A platform where neither is available refuses the
connection rather than accepting it — the check is never skipped. Both binaries
run the same implementation, in `mult-protocol::peer`.

**Files `mult` acts on.** `config.json` and `state.json` are opened
`O_NOFOLLOW` and must be regular files owned by the current user with no
group/other write bit, and are read under a size cap. This matters because their
paths are environment-overridable and their contents are executed or replayed:
the config holds shell command lines that auto-start, the state file replays
terminals. A file that fails any check is refused; only a genuinely missing file
falls back to defaults.

The per-chat agent status files are opened `O_NOFOLLOW`, must be regular files,
and are read under a 64 KiB cap, but are **not** individually owner- or
mode-checked. What protects them is the directory: every ancestor of the runtime
directory they live in is verified private — owned by us, not group- or
other-writable — before any status file is created or read, and a directory that
fails that check disables status reporting entirely rather than being used
anyway.

**`state.json` is an execution boundary.** The state file does not merely
describe the last session, it can *cause execution*: a terminal stored with
`"restore_on_launch": true` and a `command` launch holds a shell command line,
and restoring it means `$SHELL -lc <command>`. The file is reachable by anything that can write the
user's data directory — a synced dotfile repository, a shared `$XDG_DATA_HOME`,
any same-uid process — so it is treated as untrusted input:

- shell terminals restore automatically, because their program comes from
  `$SHELL` and not from the file;
- a terminal with a stored command line is **not** replayed at startup. It is
  left stopped, and a confirmation prompt shows each command verbatim before
  anything runs. Declining leaves the terminals stopped and says so.

Chat agents are unaffected: their command lines come from `config.json`, which
is the user's own configuration and is ownership-checked when read.

The file also carries a workspace `cwd` and `environment`, which are applied to
every terminal in that workspace. Those are *not* individually confirmed today,
so a hostile state file can still influence a shell that the user starts (for
example through `BASH_ENV`-style variables). Treat an untrusted `state.json` as
untrusted regardless of the confirmation.

**Files `mult` writes.** State, runtime and generated hook files are `0600`,
directories `0700`, verified against pre-created ("squatted") paths, and written
through `O_EXCL` temp files that are renamed into place. Generated hook scripts
and settings are named by content, so they are reused rather than accumulating.

**Process spawning.** An autospawned `mult-server` must be a regular file owned
by the user or by root with no group/other write bit, and is started with a
minimal environment (see [`docs/DAEMON.md`](docs/DAEMON.md)) rather than the
client's, so a long-lived daemon does not carry one session's credentials into
every later PTY.

**Repositories.** The branch shown per workspace is read from `.git/HEAD`
directly. `mult` runs no `git` subprocess, so opening a hostile repository does
not cause its `.git/config` (`include.path`, `core.fsmonitor`, `core.hooksPath`)
to be parsed.

**Daemon sessions are namespaced per client instance.** Every session on
`mult-server` belongs to the instance token its creator presented, and a
connection can only see, attach to or write to sessions in its own namespace.
The token is stored in `state.json` and is what lets a restarted client reclaim
its own panes. It raises the bar for stealing a live PTY stream from "speak the
protocol and guess a small session id" to "read this user's state file", but it
is not a capability against a same-uid attacker, who can read that file. See
[`docs/DAEMON.md`](docs/DAEMON.md).

**Daemon resource limits.** The daemon caps concurrent connections (64) and live
sessions (256), and closes an established connection that sends nothing for 120
seconds (the client sends a keepalive every 20 s). Without these, a same-uid
loop of `CreateSession` — or of idle connections — exhausted memory, PIDs, file
descriptors and threads, killing every live pane the user had.

## What is not enforced

Any process running as the same user is inside the trust boundary. It can read
`state.json` and therefore the instance token, and with the token it can attach
to that instance's live sessions (`C12` in
[`docs/BACKLOG.md`](docs/BACKLOG.md)); attach remains takeover-by-default within
an instance, because that is what a reconnecting client needs. A workspace `cwd`
and `environment` restored from `state.json` are applied without confirmation
(see the note above).

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
