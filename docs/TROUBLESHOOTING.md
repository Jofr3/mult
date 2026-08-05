# Troubleshooting

Failure modes `mult` actually produces, the message each one shows, and what to
do about it. Every message below is quoted from the current source; if you see
something not listed here, it is worth an issue.

Messages reach you in one of two places:

- **The status line**, a single row above the prompt, for problems that belong to
  no pane. Marked by shape as well as colour: `x` error, `!` warning, `·` info.
  `Ctrl+g` dismisses the current one and reveals the next.
- **Inside a pane**, prefixed `[mult]`, for problems that belong to one terminal
  or chat.

---

## The client cannot reach the daemon

### `failed to connect to mult-server: No such file or directory (os error 2)`

The socket is not there and the client did not autospawn a daemon. Autospawn is
attempted only when **all** of these hold:

- `$MULT_SERVER_AUTOSPAWN` is not `0`/`false`/`False`/`FALSE`;
- the running binary's file stem is exactly `mult`;
- a file named `mult-server` sits **next to it**, in the same directory;
- that file passes the safety check below.

Start the daemon yourself if any of those cannot hold in your setup:

```sh
mult-server &          # or: just server
```

### Autospawn silently does nothing

This is the case worth knowing about, because **the check that rejects the
daemon binary produces no message of its own** — you only see the underlying
connect error above. Since Slice 5 the client refuses to execute a `mult-server`
that is not:

- a **regular file** (`symlink_metadata` is used, so a symlink is rejected
  outright);
- owned by **you or root**;
- free of the group- and other-write bits (`mode & 0o022 == 0`).

So a `mult-server` in a world-writable directory, or one owned by another user,
or a symlink to one, is skipped rather than run. Check it:

```sh
ls -lL "$(dirname "$(command -v mult)")/mult-server"
```

Fix the ownership and mode, or start the daemon manually. This is deliberate: the
client would otherwise execute whatever that path pointed at.

If the two binaries are simply not together — you built one and copied the
other, or installed only `mult` — put them side by side. **The release archives
always contain both for this reason.**

### `could not locate mult-server next to the mult executable; run `mult-server` manually`

The binary passed the check and then disappeared before the spawn. Rare; a race
with an upgrade or a cleanup script.

### `failed to connect to mult-server after autospawn: <error>; initial error: <error>`

The daemon was spawned but no socket appeared within 15 seconds. Run
`mult-server` in a terminal to see why it is failing — usually the socket path
is unwritable, or a stale socket file is in the way.

### `timed out after 2s waiting for mult-server hello`

The socket accepted the connection but nothing spoke the protocol. Something else
is listening on that path. Check `$MULT_SOCKET_PATH` and `--socket`.

---

## Protocol version mismatch after an upgrade

`PROTOCOL_VERSION` is currently **11**. The client and daemon must agree exactly;
this is the single most common upgrade problem, because a `mult-server` started
before the upgrade keeps running.

### `failed to connect to mult-server: mult-server protocol version <n> is incompatible with client version 11; restart mult-server`

An old daemon is still running. Restart it:

```sh
pkill mult-server        # panes it owns are lost; see below
mult                     # autospawns a matching daemon, if adjacent
```

### `failed to connect to mult-server: mult-server rejected (ProtocolMismatch): client protocol version <n> is incompatible with server version 11; restart mult clients`

The other direction: a *new* daemon and an *old* client. Upgrade the client, or
restart the daemon after upgrading everything.

### `failed to connect to mult-server: mult-server rejected (InstanceTokenRequired): client did not present an instance token; upgrade the mult client`

A pre-instance-token client against a current daemon. Upgrade the client.

Note that the parenthesised code is a `RejectCode` rendered with `Debug`, so it
appears as a bare variant name. It is the machine-readable half of the rejection;
the prose after the colon may be reworded between releases, the code will not.

**Restarting the daemon kills its panes.** Every terminal and agent it owns
retires (see below). That is unavoidable across a protocol change.

---

## Terminals retired when the daemon went away

When the connection drops, every live pane is marked exited and gets a line:

```
[mult] PTY exited: terminated by mult-server connection lost
[mult] pi agent exited: terminated by mult-server connection lost
```

(or `Claude Code agent exited: …`). The sidebar glyph becomes `!`. This is
bookkeeping, not data loss in the client's durable state — workspaces, chats and
terminal definitions are all still in `state.json`. What is gone is the running
process and its scrollback, which only ever lived in the daemon.

The client reconnects on its own, retrying about once a second, and says so:

```
· reconnected to mult-server
```

Terminals do **not** restart themselves after a reconnect; select one and type,
or rely on `auto_start_terminals`. While a spawn is queued against a daemon that
is not up yet you will see `[mult] waiting for mult-server...`.

Related messages:

- `mult-server: <message>` — a connection-wide failure the daemon reported.
- `mult-server stopped reading input after 5s` — the daemon accepted the socket
  but stopped draining it. Restart it.
- `[mult] PTY exited: terminated by detached: another client attached to this pane`
  together with `pane <n> was taken over by another mult client` — a *different*
  `mult` instance claimed that pane. Sessions are namespaced by the instance
  token in each client's `state.json`, so this normally means two clients share
  one state file (a copied `$MULT_STATE_PATH`, or the same file opened twice).
  Give each instance its own `--state`.

---

## The daemon refuses the connection over its caps

The daemon is bounded on purpose. The limits are compile-time constants in
`src/bin/mult-server.rs`:

| Limit | Value |
| --- | --- |
| Concurrent clients (`MAX_CLIENTS`) | 64 |
| Live sessions (`MAX_SESSIONS`) | 256 |
| Idle deadline (`CLIENT_IDLE_TIMEOUT`) | 120 s |
| Hello deadline (`CLIENT_HELLO_TIMEOUT`) | 2 s |

### `failed to connect to mult-server: mult-server rejected (ConnectionLimit): mult-server is already serving 64 clients`

64 connections are open. Almost always leaked clients rather than 64 real
windows — check with `ss -xp | grep mult.sock` (or `lsof`), and quit the ones you
are not using. Restarting the daemon clears them, at the cost of its panes.

### `[mult] failed to create session: mult-server is already hosting 256 sessions`

Written into the pane that could not start. 256 PTYs are alive. Delete terminals
and chats you are done with; each one you leave running holds a session.

### `mult-server: connection closed after 120s with no client traffic`

The daemon dropped a connection that said nothing for two minutes. A healthy
client sends a keepalive every 20 seconds, so this indicates a client that was
stopped (`SIGSTOP`, a suspended job, a debugger) rather than an idle one. It
reconnects when it resumes.

---

## Peer credential verification failed

Both ends verify that the process on the other side of the socket runs as the
same uid, and an unavailable check is a **hard failure**, not a warning.

### `refusing <peer>: cannot determine peer uid: <error>`

The check could not run. The inner error tells you which case:

- `peer credentials are not available on this platform` — this build has no
  implementation for your OS. Linux uses `SO_PEERCRED`; macOS and the BSDs use
  `getpeereid`. Other Unixes have neither wired up.
- `short SO_PEERCRED response` — the kernel returned a truncated struct; expect
  this only under an unusual sandbox or emulation layer.

### `rejecting <peer> uid <n>; expected current uid <m>`

A process running as a different user is on the other end of the socket. Do not
work around this. It means either a genuinely shared socket path — check that
`$MULT_SOCKET_PATH` does not point somewhere world-reachable — or a daemon left
behind by another account (including one started under `sudo`). Remove the stale
socket and start a daemon as yourself.

`<peer>` reads `client` in the daemon's logs and `mult-server` on the client's
status line.

---

## A `.corrupt-*` or `.pre-11a-*` file appeared next to `state.json`

Both live beside your state file, named from the state path.

### `state.json.corrupt-<unix-seconds>-<16 hex digits>`

Your state could not be decoded at all, so it was **renamed** aside and the
session started empty:

```
· <path> could not be read (<reason>); starting empty. Your previous state was saved to <path>
```

Decoding is lenient field by field — a missing, `null` or unrecognised key costs
you that field, not the file — so reaching this means a whole workspace or chat
entry had the wrong shape. The backup is your original bytes, untouched; it is
worth keeping and worth an issue, because well-formed state should not get here.

If the rename itself fails, startup stops with
`mult: state JSON is invalid (<reason>); failed to move <path> to <path>: <reason>`
and exit status 2, rather than running with an empty state that would overwrite
the file you still have.

### `state.json.pre-11a-<unix-seconds>-<16 hex digits>`

Not corruption. Your state carried chat transcripts from a feature removed in
Slice 11a, so a **copy** (mode `0600`) was kept before they were dropped:

```
· <path> held <n> chat messages from a removed chat-transcript feature; they are no longer shown. A copy of the file was saved to <path>
```

Or, if the copy could not be written:

```
· <path> held <n> chat messages from a removed chat-transcript feature; they are no longer shown, and the copy could not be written (<error>)
```

The current file is fine either way. Delete the copy once you are satisfied
nothing was in it that you wanted.

### Other state errors, all fatal at startup as `mult: <message>`

- `cannot read <path>: <reason>` — refused by the private-file check (not a
  regular file, owned by someone else, group/other-writable, or over the 32 MiB
  cap), or plain I/O failure.
- `state file version <n> is newer than supported version <m>; not modifying <path>`
  — you ran a newer `mult` against this state file. The old client refuses rather
  than downgrading it. Use the newer client, or point `--state` elsewhere.

---

## Agent status reporting is disabled

### `agent status reporting is disabled: refusing to use runtime directory <path>: it <problem>; the chat status dot will not update`

`mult` gives each agent a private file to write its status into, under
`$XDG_RUNTIME_DIR/mult` — or, with no `$XDG_RUNTIME_DIR`,
`<tmpdir>/mult-<uid>/mult`. That directory and every ancestor is checked without
following symlinks, and `<problem>` is one of:

- `is not a directory`
- `is writable by group or others` — the usual one. Some component has mode bits
  in `0o022`.
- `is owned by another user (refusing a pre-created path)` — somebody else
  created the path first.

Chats keep working; only the live status dot stops updating, and it stays at the
value it last saw. `mult` **fails closed**: `$MULT_AGENT_STATUS_PATH` is not
exported to the agent at all rather than pointed at the rejected directory.

Fix the offending directory:

```sh
ls -ld "${XDG_RUNTIME_DIR:-/tmp}" "${XDG_RUNTIME_DIR:-/tmp}/mult"
chmod 700 "${XDG_RUNTIME_DIR:-/tmp}/mult"
```

On a system with no per-user `$XDG_RUNTIME_DIR`, setting one to a private
directory you own is the cleaner fix.

---

## `pi` or `claude` is not installed

There is no PATH check: the configured command goes to your login shell verbatim,
so you get the **shell's** message inside the chat pane, then `mult`'s exit line:

```
bash: line 1: pi: command not found
[mult] pi agent exited: exit 127
```

The chat's sidebar dot turns red (`!`, failed). Either install the binary, or
point the config at the one you have:

```json
{ "pi_agent_command": "/opt/pi/bin/pi", "claude_code_command": "claude" }
```

Because the command is run as `$SHELL -lc <command>`, anything your login shell
can resolve works — including a shell function or an alias defined in your
profile. A chat that has never produced output shows the config path to edit
right in the pane:

```
pi agent not started. Type to start it and send input.
Set `pi_agent_command`/`auto_start_pi_agent` in:
<config path>
```

---

## Config problems

### `mult: config error at <path>:<line>:<col>: <message>` (exit 2)

The file does not decode: malformed JSON, an **unknown key**, or a value of the
wrong type. Unknown keys are rejected on purpose — `auto_start_terminal` instead
of `auto_start_terminals` used to be accepted and silently ignored. Startup stops
rather than quietly running defaults, because this file names commands that get
executed.

### `mult: config error at <path>: <reason>` (exit 2)

The file could not be read at all: it is a symlink, not a regular file, owned by
someone else, writable by group or others, or over 1 MiB. See
[CONFIG.md](CONFIG.md#how-the-file-is-read).

### ``config: colorscheme.<key> is not a #rrggbb color (`<value>`); using the default``

A warning, not an error. That one key keeps its default and everything else
loads. Values are `#rrggbb`; `rrggbb` without the `#` is also accepted. An
*empty* value reports as `(empty)` rather than as empty backticks.

---

## Colours look wrong or unreadable

- **Everything is monochrome.** `$NO_COLOR` is set to a non-empty value. Per
  <https://no-color.org> the value is irrelevant — only emptiness is. `unset
  NO_COLOR` to get the palette back.
- **Washed out on a light terminal.** There is no light-theme default. Set the
  `colorscheme` keys, or set `NO_COLOR` to inherit your terminal's own colours.
- **A config change did nothing.** Colours and every other key take effect from
  "Reload config" in the command palette, without a restart. `mouse_capture` is
  the one exception — it is pushed to the host terminal at startup, so changing
  it needs a restart.

---

## The pane says `too small`

The window is too short or too narrow to draw a terminal screen. `mult`'s
emulator cannot hold a screen smaller than 2 rows by 2 columns — one row or one
column makes it overflow on ordinary output, which is an upstream defect the
size clamp works around — so a pane below that is not drawn at all rather than
being shown the top-left corner of a screen whose cursor and last line are off
the edge. A pane too narrow even for the words is left blank.

The PTY itself keeps running at 2×2 and nothing is lost. Make the window taller
or wider, or close the prompt (`Esc`) and dismiss the status line (`Ctrl+g`),
each of which gives the pane a row back.

---

## Keyboard input goes to the wrong place

- `F1` is the only key `mult` never forwards. A bare `?` opens help only when no
  chat or terminal is selected to receive it.
- `Ctrl+g` dismisses a status message when one is shown; otherwise it goes to the
  focused PTY.
- `Ctrl+k` deletes to end of line in most prompts, but keeps its "select previous
  match" meaning in the command palette and the open-workspace prompt.
- If modified keys (`Alt+Shift+<letter>`, cursor keys under an application-mode
  program) arrive wrong, your terminal's keyboard protocol matters. Include your
  terminal emulator, `$TERM`, and whether the kitty keyboard protocol is active
  when reporting it — the encoding path is terminal-specific.

---

## Reporting a bug

Please include:

- `mult --version`
- your terminal emulator and version, and `$TERM`
- OS and version
- whether the kitty keyboard protocol is active
- the exact message text

`.github/ISSUE_TEMPLATE/bug_report.yml` asks for all of this.
