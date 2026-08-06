# Troubleshooting

Symptoms `mult` and `mult-server` actually produce, what causes them, and what
to do. Every quoted message below is copied from the current source, so you can
grep for it; `{…}` marks an interpolated value.

Messages prefixed **[pane]** are written into the affected chat/terminal pane as
a system line, so you see them inside `mult` rather than on stderr. Messages
prefixed **[stderr]** come from `mult-server`, which is normally started detached
with its output going to `/dev/null` — to see them, run `just server` (or
`cargo run --bin mult-server`) in a second terminal and start `mult` with
`MULT_SERVER_AUTOSPAWN=0`.

Related: [CONFIG.md](CONFIG.md) for every config key, [DAEMON.md](DAEMON.md) for
socket and daemon design, [../SECURITY.md](../SECURITY.md) for the threat model.

---

## Everything stopped working right after an upgrade

> `mult-server protocol version {protocol_version} is incompatible with client version {PROTOCOL_VERSION}; restart mult-server`

or, in the daemon's log:

> **[stderr]** `client protocol version {protocol_version} is incompatible with server version {PROTOCOL_VERSION}; restart mult clients`

**Cause.** `mult-server` outlives the client on purpose — it keeps your PTYs
alive across client restarts — so after you upgrade `mult`, the *old* daemon is
still running and still bound to the socket. The wire protocol is versioned and
there is no cross-version compatibility: the mismatch is refused at `Hello`
rather than being papered over.

**Fix.** Stop the old daemon and let the new client spawn a new one:

```sh
pkill -f mult-server   # or kill the specific pid
mult
```

**Cost.** PTYs owned by the old daemon die with it. Anything running in a pane
is lost; the workspace/chat/terminal metadata in your state file is not.

**Avoiding it.** Ship and install both binaries together — every release archive
contains both for this reason. Upgrading only `mult` guarantees this error.

---

## "could not locate a trusted mult-server", or panes never start

> ``could not locate a trusted mult-server next to the mult executable; run `mult-server` manually``

or

> `failed to connect to mult-server after autospawn: {wait_error}; initial error: {error}`

**Cause.** The client does not search `$PATH` for the daemon. It takes its own
`current_exe()`, requires the file stem to be exactly `mult`, replaces the file
name with `mult-server`, and then checks that the resulting binary is safe to
execute. Four things break this — note the word **trusted** in the message
covers the last two:

- **`mult-server` is not in the same directory.** Common when you `cp
  target/release/mult ~/.local/bin/` and leave the daemon behind, or when a
  package installs only the client.
- **The client binary was renamed** (a wrapper called `mult-dev`, a symlink
  resolved to a different stem). Autospawn is then not attempted at all and you
  get the *original* connection error (typically "No such file or directory")
  with no mention of autospawn.
- **The `mult-server` binary is not owned by you or by root**, is not a regular
  file, or is **group- or other-writable**. `mult` is about to execute it; a
  binary another local user can rewrite is a way to run their code as you.
- **The directory containing it fails the same check.** A writable directory
  means the *name* can be replaced, which is the identical attack one level up.

Symlinks are followed on purpose — Nix profiles and `cargo install` shims
legitimately link into a store — and both the link's directory and the target
are validated.

Autospawn is also skipped, by design, when the socket exists and something is
listening but refusing, when the connect error is anything other than
`NotFound`/`ConnectionRefused`, or when `MULT_SERVER_AUTOSPAWN=0` is set.

**Fix.** Install both binaries into the same directory, owned by you, not
group-writable:

```sh
install -m755 target/release/mult target/release/mult-server ~/.local/bin/
ls -ld ~/.local/bin ~/.local/bin/mult-server   # no `w` for group or other
chmod go-w ~/.local/bin ~/.local/bin/mult-server
```

A common trip-up is a shared directory such as `/usr/local/bin` left at mode
`0775` with a group other people are in. Fix the permissions rather than working
around the check.

Or run the daemon yourself and let the client find the socket:

```sh
mult-server &        # in another terminal
MULT_SERVER_AUTOSPAWN=0 mult
```

---

## A `state.json.corrupt-…` file appeared and my workspaces are gone

**Cause.** The state file did not decode as valid JSON for the current schema.
Rather than overwrite it, `mult` renames it aside to
`state.json.corrupt-{unix-seconds}-{16 random hex digits}`, mode `0600`, and
starts from defaults.

**This is currently silent.** Nothing in the UI tells you it happened — you
notice because your workspaces are missing. (Surfacing it is `E11` in
[BACKLOG.md](BACKLOG.md).) The backup is your copy of the old state:

```sh
ls -l "${XDG_DATA_HOME:-$HOME/.local/share}/mult/"
```

**Fix.** Inspect the backup, repair the JSON, and move it back over
`state.json` **while `mult` is not running** (a running client holds the lock and
will overwrite on its next save).

Two related errors do surface:

> `state JSON is invalid ({decode_error}); failed to move {path} to {backup}: {rename_error}`

The state was unreadable *and* could not be moved aside — usually a read-only or
full filesystem. `mult` refuses to start rather than discard it.

> `could not choose a unique corrupt-state backup name`

Sixteen random names in a row already existed. Clean out old `.corrupt-*` files.

Note that `*.corrupt-*` is in `.gitignore`, so a backup created inside a
repository will not be committed by accident.

---

## "state file version … is unsupported", or "another mult process owns state path"

> `state file version {version} is unsupported (current version is {STATE_VERSION}); not modifying {path}`

**Cause.** The state file was written by a **newer** `mult`. Downgrading is not
supported, and the file is deliberately left byte-for-byte untouched rather than
migrated backwards or reset.

**Fix.** Use the newer `mult`, or point the old one somewhere else with
`MULT_STATE_PATH=/tmp/old-state.json mult`.

> `another mult process owns state path {path}`

**Cause.** State ownership is a process-lifetime `flock`, taken before loading.
A second TUI on the same state path fails immediately instead of racing and
losing one of the two snapshots. Note that a *crashed* client does not leave the
lock held — the kernel releases it — so this really does mean another live
process.

**Fix.** Use the existing instance, or give the second one its own state:
`MULT_STATE_PATH=~/.local/share/mult/second.json mult`.

---

## "rejecting … uid …; expected current uid …"

> `rejecting mult-server uid {peer_uid}; expected current uid {current_uid}` (seen by the client)
>
> **[stderr]** `rejecting client uid {peer_uid}; expected current uid {current_uid}` (seen by the daemon)

**Cause.** Both ends verify the Unix-socket peer's credentials and require the
same effective UID. The socket carries keystrokes and PTY output, so a peer that
is not you is refused. Realistic triggers:

- a socket path shared between accounts (`MULT_SOCKET_PATH=/tmp/shared.sock`);
- running the client under `sudo`/`doas` while the daemon runs as you, or the
  reverse;
- a daemon left running by a different account on a multi-user machine.

You may also see:

> `short SO_PEERCRED response`

which means the kernel returned a truncated credential structure — treated as a
failure, never as a pass.

**Fix.** Run both processes as the same user, and give each user their own
socket. The default path is already per-user: `$XDG_RUNTIME_DIR/mult.sock`, or
`/tmp/mult-<euid>/mult.sock` when `XDG_RUNTIME_DIR` is unset — keyed on the
effective UID, not on `$USER`.

**Caveat.** Peer verification is enforced on Linux. On macOS and the BSDs the
credential lookup currently returns "unknown" and is treated as accept, so there
the socket's `0600` mode and its `0700` parent are the only barrier. Tracked as
`C3`; do not put the socket somewhere world-reachable on those platforms.

---

## A pane says the session is unavailable after restarting the daemon

> **[pane]** ``command terminal `{name}` was not relaunched because its daemon session is unavailable; type or use Start selected PTY to start it deliberately``
>
> **[pane]** ``failed to restore terminal `{name}` without relaunching it: {error}``
>
> **[pane]** `agent session is unavailable; it was not relaunched during restoration`
>
> **[pane]** `failed to restore agent without relaunching it: {error}`
>
> and in the pane body: `Command was not restored or auto-started. Type or use Start selected PTY to run it deliberately.`

**Cause.** This is intended behaviour, not a failure. A **command** terminal
(one created with an explicit command rather than a shell) is persisted with its
command line. On startup `mult` will *attach* to its still-live daemon session,
but it will never *relaunch* the command — re-running `terraform apply` or
`make deploy` because a daemon restarted is not a decision a multiplexer gets to
make for you. The same rule covers agent chats.

Shell terminals have no such command and are restarted normally.

**Fix.** Select the pane and type, or use **Start selected PTY** from the
command palette (`Ctrl+p`). That is the deliberate action the message asks for.

You may also see the exit reported as:

> **[pane]** `PTY exited: terminated by server session unavailable`

which is how the client reports panes it can no longer reach when the daemon
goes away underneath it.

---

## An agent chat starts and immediately dies (`pi` / `claude` not installed)

**Symptom.** The pane shows the shell's own error and an exit line:

```
/bin/sh: line 1: pi: command not found
```

> **[pane]** `PTY exited: exit 127`

or, for an agent chat, `pi agent exited: exit 127` / `Claude Code agent exited: exit 127`.

**Cause.** `pi_agent_command` and `claude_code_command` are run through your
login shell (`$SHELL -lc "<command>"`). `mult` does not check that the binary
exists first — the shell does, and reports it in the pane. Exit 127 is the
shell's conventional "command not found".

**Fix.** Install the agent, or point the config at the real path:

```json
{ "pi_agent_command": "~/.local/bin/pi", "claude_code_command": "claude" }
```

Because the command goes through a **login** shell, it also sees your `~/.profile`
/ `~/.zprofile` `PATH` — but not your *interactive* rc file. A binary that only
exists on the `PATH` set in `~/.bashrc` will not be found. See
[CONFIG.md](CONFIG.md).

**Known cosmetic bug.** The idle placeholder in an *empty* chat pane reads
"Pi agent not started. Type to start it and send input." and points at
`pi_agent_command`/`auto_start_pi_agent` **for Claude Code chats too**. The chat
is a Claude Code chat (the sidebar shows `agent: cc`) and the keys you actually
want are `claude_code_command`/`auto_start_claude_code_agent`. Tracked as `F18`.

---

## "Input rejected for pane N", or "too many pending requests"

> **[pane]** `Input rejected for pane {n}: NotOwner`
>
> **[stderr]** `refusing PTY input for pane {n}: writer queue {refusal}`

**Cause.** Each pane has a bounded write queue (1 MiB) drained by a dedicated
writer thread. If the program in the pane has stopped reading its standard
input and you keep typing or paste something large, the queue fills. The daemon
then **refuses** the write and says so, rather than blocking — a blocking write
under the daemon lock used to freeze every pane and every client. The refusal is
pane-scoped: your connection and every other pane keep working.

The keystrokes were definitely *not* delivered. They are not silently retried,
because replaying input into a program that may have consumed part of it is
worse than dropping it.

`NotOwner` is also the reason reported when the daemon is shutting down or the
pane is already stopping. Other reasons on the same message are `PaneMissing`
(the pane is gone) and `StaleLease` (another client took the pane over — you
will have had a takeover event first).

**Fix.** Unblock or stop the program in the pane.

Other capacity refusals, all `Error`-level and all meaning "the daemon is at a
hard limit, not broken":

> `too many pending requests` — more than 1024 correlated requests in flight on
> one connection. Practically only reachable by a client bug or a script hammering
> the socket.
>
> `attachment lease space exhausted` · `client ID space exhausted` ·
> `session ID space exhausted` — a monotonic ID counter reached its maximum
> rather than being allowed to wrap and alias an existing lease or session. If
> you see one of these, please file a bug.

**Fix.** Restart the daemon.

---

## Agent status dots never update, or a chat goes straight to failed

> **[pane]** `failed to prepare private agent status journal: refusing to use runtime directory {path}: it is writable by group or others`
>
> **[pane]** `failed to prepare private agent status journal: refusing to use runtime directory {path}: it is owned by another user (refusing a pre-created path)`
>
> **[pane]** `failed to prepare private agent status journal: refusing to use runtime directory {path}: it is not a directory`

**Cause.** The sidebar status dot (thinking / waiting / done / failed) is driven
by a small append-only journal that the agent's own lifecycle hook writes. That
journal lives under `$XDG_RUNTIME_DIR/mult/status-v1/`, and `mult` walks the
directory chain verifying every level is a directory, owned by you, and not
group- or other-writable — because anything that can write there can forge agent
status. If the check fails, the chat is marked **failed** and the agent is not
launched at all.

The usual causes are a `$XDG_RUNTIME_DIR` pointing at a shared directory, a
`/tmp/mult-<uid>` left behind by another account, or an over-permissive
`chmod` on the runtime directory.

**Fix.** Inspect and correct the chain:

```sh
namei -l "${XDG_RUNTIME_DIR:-/tmp/mult-$(id -u)}/mult/status-v1"
chmod 700 "${XDG_RUNTIME_DIR:-/tmp/mult-$(id -u)}/mult"
```

Remove a `/tmp/mult-<uid>` owned by someone else (you cannot, so ask an admin —
that is exactly the squatting case the check exists to catch). A sticky,
world-writable system root such as `/tmp` itself is accepted; the private subtree
below it is what must be yours.

The same "refusing to use runtime directory" error also blocks the **socket**
parent, in which case `mult-server` fails to start rather than binding somewhere
writable by others.

Related: even when the journal is fine, a `pi`/Claude Code session whose command
was heavily customised may not run the bundled hook at all, in which case the dot
stays grey. That is a known incomplete area (`6.3` in
[ROADMAP.md](ROADMAP.md#open-decisions-carried-over)).

---

## "cannot determine a durable … directory"

> `cannot determine a durable state directory: set an absolute HOME or the relevant XDG directory`
>
> `cannot determine a durable configuration directory: set an absolute HOME or the relevant XDG directory`

**Cause.** `mult` resolves state and config from an absolute `$XDG_DATA_HOME` /
`$XDG_CONFIG_HOME`, then an absolute `$HOME`, then the effective user's passwd
entry. If all three are missing or relative — typical inside a minimal container,
a systemd unit without `HOME=`, or a `su` without `-` — it **fails** instead of
writing your state into whatever the current directory happens to be.

**Fix.** Set one of them, or override the path directly:

```sh
HOME=/root mult
# or
MULT_STATE_PATH=/var/lib/mult/state.json MULT_CONFIG_PATH=/etc/mult/config.json mult
```

---

## "Too many levels of symbolic links" at startup (symlinked config)

> `Error: Os { code: 40, kind: FilesystemLoop, message: "Too many levels of symbolic links" }`

or, when a **directory** on the way to the config is the link:

> `Error: Os { code: 20, kind: NotADirectory, message: "Not a directory" }`

**Cause.** `config.json` is now read with the same discipline as the state file,
and for a sharper reason: `pi_agent_command` and `claude_code_command` are handed
to `$SHELL -lc` and auto-started by default, so whoever controls those bytes runs
code as you with no keystroke. Every path component is opened with `O_NOFOLLOW`,
so a symlinked `config.json` — or a symlinked directory anywhere above it — is
refused, and the link's target is never read. Neither `$MULT_CONFIG_PATH` nor
`$XDG_CONFIG_HOME` is a way around this; both steer the same checked path.

These two messages are the raw `openat` failures rather than one of `mult`'s own,
which is why they name neither the config nor the path: an `O_NOFOLLOW` open of a
symlink is `ELOOP`, and an `O_NOFOLLOW|O_DIRECTORY` open of one is `ENOTDIR`.

**Symlinked config files are no longer supported.** That is the layout most
dotfile managers produce — GNU stow always links, and a bare-repo setup or
chezmoi in symlink mode can — so a config that worked before this change now
fails startup instead of being read.

**Fix.** Copy the file rather than linking it, and keep the directory private:

```sh
namei -l ~/.config/mult/config.json    # shows which component is the link

rm ~/.config/mult/config.json
cp ~/dotfiles/mult/config.json ~/.config/mult/config.json
chmod 700 ~/.config/mult
chmod 600 ~/.config/mult/config.json
```

Or leave the repository copy authoritative and point at it as a real file:

```sh
MULT_CONFIG_PATH=~/dotfiles/mult/config.json mult
```

That works as long as the repository copy is itself a regular file of yours in a
directory of yours that is not group- or other-writable. For stow, the practical
answer is to stop stowing this one file.

**The other rejections on the same read**, all startup errors and all quoted from
source:

> `config file {path} is not a regular file`
>
> `config file {path} is not owned by the effective user`
>
> `config file {path} has multiple hard links` — a hard link, unlike a symlink,
> is indistinguishable from the file, so link count is checked instead.
>
> `config file {path} could not be restricted to owner-only access`
>
> `config file {path} exceeds 1048576 bytes`

and, for the containing directory:

> `state parent is writable by group or others, so its lock inode is replaceable: {path}`
>
> `private state directory is not owned by the effective user: {path}`
>
> `state parent is not a directory: {path}`

Those last three say **state** even when it is the *config* directory that
failed: state and config share one hardened read implementation and its messages
were written for state. Check the path in the message, not the noun.

Note that the mode of the config file itself is repaired rather than refused — a
`0644` config is `chmod`ed to `0600` as it is read — so only a mode that could
not be tightened produces the "owner-only access" error. See
[CONFIG.md](CONFIG.md#the-file).

---

## An agent can't see its API key (or any other exported variable)

**Symptom.** `ANTHROPIC_API_KEY` is exported in the shell you started `mult`
from, but the `pi` or Claude Code process in the pane behaves as though it is
unset — a login prompt, an auth error, a 401. The same applies to any variable
you export for an agent: `OPENAI_API_KEY`, `GH_TOKEN`, `HTTPS_PROXY`.

**Cause.** An **autospawned** `mult-server` does not inherit your environment. It
is spawned with `env_clear()` plus an allow-list — `PATH`, `HOME`, `SHELL`,
`USER`, `LOGNAME`, `TERM`, `LANG`, everything prefixed `LC_`, and everything
prefixed `MULT_` — and the daemon then hands *its* environment to every PTY it
spawns, for every client that connects afterwards. Anything outside the list is
already gone by the time your agent starts.

The list exists because of that outliving: without it, the first shell that ever
ran `mult` would re-export its secrets into every pane of every later session.
[DAEMON.md](DAEMON.md) has the design.

A **manually started** `mult-server` is unaffected — it keeps whatever
environment its operator gives it.

**Fix.** Start the daemon yourself with the variable exported:

```sh
export ANTHROPIC_API_KEY=...
just server                    # or: mult-server
MULT_SERVER_AUTOSPAWN=0 mult
```

Or export it where the pane's own shell will read it. Agent commands and command
terminals run as `$SHELL -lc`, so an export in `~/.profile` / `~/.zprofile`
reaches the pane even under an autospawned daemon. An export in `~/.bashrc` does
not — that is an interactive rc file, and this shell is a login shell.

Restarting `mult` on its own changes nothing: a daemon that is already running is
not respawned, and it keeps the environment it was started with. Stop it first
(`pkill -f mult-server`), accepting that its PTYs die with it.

---

## Keys reach the wrong place, or the mouse does nothing

Not an error message, but the most common class of "it looks broken":

- **Modified keys go to the wrong program.** `mult` reserves a handful of
  `Ctrl` chords (see the README's control table); everything else is forwarded.
  If a key is being eaten, it is a reserved chord. A leader/passthrough mode is
  planned but does not exist yet.
- **The wheel scrolls the wrong thing.** When the program in the pane has
  enabled mouse reporting (Claude Code, `nvim`, `less`), the wheel is forwarded
  to it. When it has not, the wheel scrolls `mult`'s own scrollback. An
  alternate-screen app has no `mult` scrollback, so a pane that "won't scroll"
  usually means the program is not reporting the mouse.
- **Selection and copy do nothing.** Both need `mouse_capture: true` (the
  default). With mouse capture off you get your emulator's native selection
  instead, which is often what you want.
- **Copy silently fails.** Copy uses OSC 52. Terminals disable it by default
  (`xterm` needs `allowWindowOps`/`disallowedWindowOps`, `tmux` needs
  `set -g set-clipboard on`). There is no fallback and no failure notice yet.
  Check `clipboard_osc52` too: with it set to `false` no escape is emitted at
  all, so selection highlights and `Ctrl+Shift+C` is still swallowed while
  nothing reaches the clipboard. See [CONFIG.md](CONFIG.md#top-level-keys).

When reporting an input bug, include your terminal emulator, `$TERM`, and
whether the kitty keyboard protocol is active — the bug template asks for all
three because the encoding differs on each.

---

## Getting more information

```sh
# Run the daemon in the foreground so its stderr is visible.
mult-server

# ... and stop the client from spawning its own.
MULT_SERVER_AUTOSPAWN=0 mult

# Use throwaway state and config to rule out a bad file. A config path that
# does not exist is not an error — mult starts on its defaults — but the
# *directory* must be yours and not group- or other-writable, so /tmp itself
# will not do: `... is not owned by the effective user: /tmp`.
scratch=$(mktemp -d) && chmod 700 "$scratch"
MULT_STATE_PATH=$scratch/state.json MULT_CONFIG_PATH=$scratch/config.json mult

# Use an isolated socket so an existing daemon is not involved.
MULT_SOCKET_PATH=$(mktemp -u) mult
```

If none of the above matches, open a bug with the template in
`.github/ISSUE_TEMPLATE/bug_report.yml`.
