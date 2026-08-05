# `mult-server` daemon

`mult-server` is the long-lived process that owns PTYs. The `mult` TUI client connects to it over a Unix socket so terminal sessions can survive client restarts.

## Socket path

Path selection comes from `mult-protocol::default_socket_path()`:

1. `$MULT_SOCKET_PATH`, when set.
2. `$XDG_RUNTIME_DIR/mult.sock`, when `XDG_RUNTIME_DIR` is set.
3. `/tmp/mult-<uid>/mult.sock`, where the fallback directory is private to the user.

The server creates missing socket parent directories with mode `0700`, binds with a restrictive umask, and sets the socket file to `0600` after binding.

## Peer verification

Both ends verify that the process on the other side of the socket runs as the same effective UID, and both use the same implementation (`mult_protocol::peer::verify_peer_is_self`): `SO_PEERCRED` on Linux, `getpeereid(3)` on macOS and the BSDs. This is not a Linux-only check, and it is not skippable — a platform where the kernel exposes neither interface refuses the connection instead of accepting it. Mode `0600` on the socket file is the first line of defence, but `$MULT_SOCKET_PATH` can point at a directory another user can write, so the path alone is not relied on.

## Autospawn

The client attempts to autospawn `mult-server` when:

- the socket is missing, or the socket path is a stale Unix socket that refuses connections;
- `MULT_SERVER_AUTOSPAWN` is not `0`, `false`, `False`, or `FALSE`; and
- a `mult-server` executable exists next to the running `mult` executable, is a regular file (not a symlink), is owned by the current user or by root, and is not writable by group or others.

Set `MULT_SERVER_AUTOSPAWN=0` to require starting the server manually. A daemon that fails the ownership/mode check is not spawned; start it manually instead.

### Autospawn environment

An autospawned daemon does **not** inherit the client's environment. It receives only `PATH`, `HOME`, `SHELL`, `USER`, `LOGNAME`, `TERM`, `LANG`, and any `LC_*` or `MULT_*` variable, plus `MULT_SOCKET_PATH` for the socket it should bind.

The reason is lifetime, not secrecy alone: the daemon outlives the client that started it, and the environment it is born with becomes the base environment of every PTY it later spawns — for every client, workspace and terminal. Inheriting everything froze the first client's API keys and agent sockets into shells started days later from unrelated projects. Anything a workspace genuinely needs is set per session through the workspace environment, which is applied on top. A daemon started manually (`just server`) still inherits the shell that started it.

## Protocol and compatibility

The client and server exchange a protocol hello and require matching `PROTOCOL_VERSION`. A protocol mismatch usually means an old server is still running after an upgrade; stop it and start the new `mult-server` binary.

The hello also carries an **instance token**, and a hello without one is refused.

`PROTOCOL_VERSION` is **11**. Version 11 added a machine-readable `RejectCode` to `ServerMessage::Error` (see below). Version 10 removed four `ClientMessage` variants no client ever sent — `Paste`, `Scroll`, `ScrollToTop` and `ScrollToBottom`. Pasting has always travelled as `Input` (the client brackets it locally), and scrolling is entirely client-side, against the local emulator's scrollback. Version 10 also collapsed `PaneId` into `SessionId`: a session owns exactly one pane and the two ids were always the same number, so `SessionInfo` no longer repeats it in a `pane` field.

IPC messages are length-prefixed and encoded with `postcard`. Oversized frames are rejected before allocation, empty frames are rejected as malformed, and the payload buffer grows as bytes arrive rather than being sized from the declared length, so a peer that sends only a length header cannot make the reader commit that much memory.

### Failure reports and `RejectCode`

`ServerMessage::Error` carries a `code: RejectCode` alongside its `message`. **The code is the contract; the message is prose for the user and may be reworded at any time.** A client must branch on the code and never on the text.

| Code | Meaning |
|------|---------|
| `HelloRequired` | A message other than `Hello` arrived before the protocol hello. |
| `ProtocolMismatch` | The client's `PROTOCOL_VERSION` does not match the daemon's. |
| `InstanceTokenRequired` | The hello carried no instance token. |
| `InstanceMismatch` | A second hello tried to move an established connection into a different session namespace. |
| `ConnectionLimit` | The daemon is already serving its maximum number of connections. |
| `SessionLimit` | The daemon is already hosting its maximum number of live sessions. |
| `UnknownSession` | The request named a session this connection's namespace does not hold. |
| `SessionBusy` | The pane was taken over by another connection of the same instance. Sent to the connection that lost it. |
| `InputRefused` | A pane's input queue is full because its child stopped reading stdin. |
| `SessionCreateFailed` | A duplicate id, the session cap, or a PTY that could not be opened or spawned. |
| `PaneOperationFailed` | Attach, resize, write or stop failed on an existing pane. |
| `Unspecified` | A failure with no more specific code. |

This exists because the client used to recover the *kind* of a failure by substring-matching the daemon's rendered message — `message.contains("already attached")` — so a `format!` string in `mult-server.rs` was a load-bearing part of the wire contract. It had already silently broken: the takeover behaviour introduced with session namespacing stopped producing that wording, and nothing failed.

`ServerMessage::Error` also carries an optional `pane`. The server sets it whenever the failing pane is known, so the client can attribute the failure to that pane; an error with no pane is connection-wide and belongs to a global surface, not to a pane. Panes the client has not attached are unknown to it: it drops output for them rather than inventing a terminal.

The client partitions the session id space it chooses from: a durable workspace terminal keeps its own id, and a chat's agent PTY sets the high bit. That encoding lives in `mult_protocol::SessionId::{for_kind, split}`, and `split` is fallible — a pane id of `0` (in either half) is malformed and is rejected rather than read as terminal `0`.

## Session namespaces (instance tokens)

Wire session ids are chosen by the client from its own `TerminalId`s, so they are only unique within one state file. The daemon therefore keys every session on the pair `(instance, session)`, where `instance` is a 64-bit token the client presents in its hello.

- The token is allocated on first use, read from `/dev/urandom` where available, and stored in `state.json` (the `instance` field). A restarted client presents the same token and so reclaims exactly the panes it left behind — the reason the daemon exists.
- A connection can only see, attach to, resize, write to or stop sessions in its own namespace. Another instance's identically-numbered session is invisible: `Attach` answers `PaneExited`, and `ListSessions` never mentions it.
- Two `mult` windows, two state files, or two users' worth of state therefore no longer collide. Before this, the second instance asking for session 1 was handed the first instance's shell and then evicted its owner.
- Deleting `state.json` (or pointing `--state` somewhere new) starts a new namespace: the old sessions keep running on the daemon but are no longer reachable, and the daemon's session cap eventually refuses new ones. Stop the daemon to reclaim them.

The token is not a secret against a same-uid attacker, who can read the state file; the trust boundary is still the uid (see [`SECURITY.md`](../SECURITY.md)). What it does buy is that a same-uid process which merely speaks the protocol can no longer enumerate or steal a live PTY stream by guessing small session ids.

## Limits

The daemon is bounded so that one runaway or hostile same-uid client cannot take out every pane the user has:

- at most 64 concurrent client connections; an over-cap connection is answered with an `Error` and closed;
- at most 256 live sessions across all instances; an over-cap `CreateSession` is answered with an `Error` and the connection keeps serving;
- an established connection that sends nothing at all for 120 s is closed. This is not an activity requirement: an attached client with no PTY traffic for hours is normal, and the client sends a `Ping` every 20 s. Silence past the deadline means the peer is gone in a way the socket never reported. The sessions are untouched — the client reconnects and re-attaches.

## PTY input

Each pane owns a writer thread fed by a bounded queue (64 chunks). The daemon's socket reader only ever enqueues, and never writes to a PTY master itself.

This is a correctness requirement, not a performance one. Writing to a master blocks with no upper bound when the child stops reading its stdin, so doing it on the reader thread stopped the daemon reading the socket, the socket buffer then filled, and the client's render thread blocked in its own `write_all` — both ends hung indefinitely, reproducibly, by pasting a large buffer into such a pane.

When a pane's queue is full its child is not consuming input, and further input for that pane is refused with a `ServerMessage::Error` naming the pane (rendered into that terminal by the client). Input is never dropped silently, and the reader thread is never blocked.

## PTY lifecycle

Session IDs are reserved under the server lock before spawning PTYs, so duplicate requested IDs cannot race with creation. If spawning fails, the reservation is released. The client waits for attach confirmation and rolls back local attachment state if attach is rejected.

Stopping a pane signals the whole terminal — the pane shell's process group and the terminal's current foreground process group — with SIGHUP, then SIGKILL for anything still alive after a short grace period. Killing only the pane shell would leave grandchildren holding the PTY slave open, so the pane's reader thread would never reach EOF.

Panes are single-attach with takeover *within an instance*: when a connection attaches to a session another connection of the same instance already holds, the previous one is told (an error plus `PaneExited` for that pane) instead of being dropped in silence. That is what lets a reconnecting client re-attach while its previous, not-yet-reaped connection is still listed. A connection presenting a different instance token cannot reach the session at all, so it cannot take it over.

Each pane retains a bounded window of raw PTY output (5 MiB) for replay on attach. The history is stored in chunks, so trimming it costs time proportional to the bytes dropped rather than to the bytes kept, and a replay shares those chunks instead of copying the history under the pane lock.

## Operational notes

- Start manually with `just server` or `cargo run --bin mult-server`.
- The server ignores SIGHUP so PTYs are not torn down when the launching terminal closes.
- Socket collisions with non-socket files are refused rather than removed.
