# `mult-server` daemon

`mult-server` is the long-lived process that owns PTYs. The `mult` TUI client connects to it over a Unix socket so terminal sessions can survive client restarts.

## Socket path

Path selection comes from `mult-protocol::default_socket_path()`:

1. `$MULT_SOCKET_PATH`, when set.
2. `$XDG_RUNTIME_DIR/mult.sock`, when `XDG_RUNTIME_DIR` is set.
3. `/tmp/mult-<uid>/mult.sock`, where the fallback directory is private to the user.

The server creates missing socket parent directories with mode `0700`, binds with a restrictive umask, and sets the socket file to `0600` after binding. Linux builds also verify that connected peers have the same effective UID.

## Autospawn

The client attempts to autospawn `mult-server` when:

- the socket is missing, or the socket path is a stale Unix socket that refuses connections;
- `MULT_SERVER_AUTOSPAWN` is not `0`, `false`, `False`, or `FALSE`; and
- a `mult-server` executable exists next to the running `mult` executable.

Set `MULT_SERVER_AUTOSPAWN=0` to require starting the server manually.

## Protocol and compatibility

The current wire protocol is **version 10**. Version 10 is intentionally incompatible with version 9: it preserves version 9's request correlation, resumable scopes, attachment leases, ordered replay, and lifecycle guarantees while adding durable state namespaces, immutable logical session tokens, and generation-safe daemon agent status. Client and server exchange a hello and require the same `PROTOCOL_VERSION`. After upgrading, stop any old daemon and start the new `mult-server` binary.

The server hello identifies both the daemon instance and a server-issued client request scope. A reconnect may resume its previous scope only when the same daemon still retains it. An idempotent request may be retransmitted only after the client confirms both the same daemon instance and the same resumed scope.

IPC messages are length-prefixed and encoded with `postcard`. Oversized frames are rejected before allocation. `ServerMessage::Error` is reserved for handshake, framing, and other connection-wide failures; create, attach, stop, and agent-status failures use their correlated result messages.

### Correlated requests and retries

`CreateSession`, `Attach`, `Stop`, `UpdateAgentStatus`, and `GetAgentStatus` carry a non-zero `RequestId`. Their result messages echo that ID, so output or a failure for pane A cannot complete pane B's request.

Request IDs increase without wrapping inside one client scope. The client bounds in-flight requests, and the daemon retains a bounded result cache per resumable scope:

- an exact retry of the same ID and body receives the original result;
- reusing an ID for a different operation or body returns a request-collision error without mutation;
- retrying an ID older than the retained cache returns retry-expired without mutation;
- an overload rejection consumes and caches the ID, so it cannot later become a mutating request.

Create is idempotent only for an exact request retry, including the identity and agent-generation fields. A new create request for an existing requested session ID returns `SessionAlreadyExists` only when the logical identity matches; another namespace or token receives a structured identity mismatch and launches nothing. Reusing a request ID with changed identity fields is a request collision. Attach exact retries preserve their original lease while that attachment remains valid. A detached, exited, or taken-over cached attach returns `Superseded`. Stop exact retries replay their result, and a new stop for an absent pane returns `AlreadyAbsent`, which is confirmed success.

A client does not replay an unresolved correlated request when the daemon instance changed or its scope was not resumed. It retains local attachment state until a correlated result or later attach reconciliation proves the pane's state.

## Durable session identity and namespaced listing

Every daemon session stores a `SessionIdentity` consisting of a namespace and immutable per-session token. Durable callers provide the random, persistent namespace/token from state. The routing `SessionId` and `PaneId` remain numeric daemon coordinates, not durable authority; a caller may request a coordinate for adapter compatibility, while omitted IDs are daemon-allocated:

- `ListSessions` requires a namespace and returns only sessions in that namespace; each `SessionInfo` includes its complete identity.
- `CreateSession` carries the complete identity. Numeric-ID or identity collisions are rejected before PTY spawn, so an alternate/copied/reset state cannot relaunch over another logical session.
- `Attach` verifies namespace and token before lease allocation, resize, takeover, or replay.
- `Stop` verifies namespace and token before lifecycle or lease processing.
- namespace and token failures are operation-scoped structured `IdentityMismatch` results; they do not close an otherwise usable connection.

The daemon indexes both numeric session IDs and complete logical identities and removes both live indexes in the same exactly-once finalization transition. Agent-status tombstones are retained separately for the daemon process lifetime as described below.

The production runtime registers every durable chat/terminal identity before create, attach, reconnect, or stop. An unregistered production operation is rejected locally; there is no PID/client-scope identity fallback. Numeric routing IDs may still be requested for compatibility with the version-9 adapter layout, but they are never authority: every daemon mutation verifies the complete namespace/token first. Restoration is attach-only: a token that has no matching version-10 daemon session is reported missing and never causes `CreateSession` or command execution.

## Generation-safe agent status

Version 10 provides daemon-owned `UpdateAgentStatus` and `GetAgentStatus` operations. Agent session creation may register metadata containing status schema version, durable chat ID, agent kind, and a random non-zero generation. Each update repeats that metadata plus the complete session identity; the daemon rejects:

- an unsupported status schema;
- a wrong namespace or token;
- a non-agent session;
- a wrong chat or agent kind;
- a stale generation.

Accepted status is retained independently of a client connection and can be retrieved after that client crashes and reconnects. Failed and exited states are final: a later running, waiting, finished, or idle update cannot overwrite them. `Finished` means one turn completed and is deliberately non-final because the long-lived PTY may accept another prompt. Child finalization records `Exited` for a successful agent child or `Failed` for a non-zero exit unless an earlier final status already exists. Final status remains queryable after the live PTY is removed, until the daemon exits or a new generation for that same immutable identity is published.

These status operations use the same scoped request cache as create/attach/stop. Exact retries replay their result, changed request bodies collide, stale request IDs expire, and overload consumes and caches the request ID. Status queries also carry the expected generation, so reconnect code cannot accidentally accept a tombstone for another process incarnation.

Pi and Claude Code hooks use generation-scoped append-only JSONL files only as untrusted ingress because those processes do not speak postcard IPC. Paths are PID-independent and derived from the durable namespace/token plus random generation under the private runtime directory. Each bounded record repeats schema, namespace, token, chat, backend, and generation; the client opens without following symlinks, requires an owner-only regular single-link file, tolerates only a truncated final record, and forwards complete validated transitions to the daemon. Files are removed when the connected TUI observes PTY finalization, and inactive generations are count-rotated. Tombstone-only recovery currently clears the durable active generation and leaves the obsolete journal for that bounded rotation; production reconnect/finalization cleanup coverage remains Phase 3 follow-up work. Shared generated extension/hook artifacts use fixed versioned names, so command construction does not leak unbounded PID/random files. Active bridge files remain across a normal TUI disconnect because the daemon-owned agent must keep reporting for a reconnecting TUI.

## Attachment ownership leases

Every successful attach returns an opaque, non-zero lease. One client connection may own multiple panes, but each pane has exactly one current `(scope, connection, lease)` owner.

Input, paste, resize, detach, and stop all require the pane's current lease **and its active bound connection**. Validation and mutation are serialized with takeover and lifecycle transitions. A token from another pane, another connection, an inactive transport, an older generation, or a pane that is stopping is rejected without closing an otherwise usable connection.

A new attach intentionally takes control:

1. the old owner is invalidated;
2. the old connection receives `TakenOver` with its displaced lease;
3. the new attach/replay transaction is delivered;
4. only the selected new connection can mutate the pane.

Thus the displaced connection cannot input, paste, resize, detach, or stop the pane. Transport loss alone does not invalidate logical ownership: if output or foreground-process delivery fails, the connection becomes inactive while `(scope, lease)` remains dormant. A different connection may bind that same lease only by repeating the exact cached `Attach` after the same daemon confirms that the scope was resumed; ordinary mutations never rebind it. Explicit detach, takeover, finalization, or a conclusive lease rejection does invalidate it.

## Attach replay ordering

A successful attach is one ordered transaction for one request and lease:

1. `AttachResult::Attached`;
2. `ReplayBegin`;
3. zero or more `ReplayChunk` messages;
4. `ReplayEnd`;
5. live `PtyOutput`.

Output sequences are absolute byte offsets. If a frame starts at `sequence`, the next frame starts at `sequence + bytes.len()`. `ReplayBegin` snapshots an exclusive `watermark`; replay covers exactly `[first_sequence, watermark)`, and the first live frame starts at `watermark`. The daemon holds the pane barrier while it queues acknowledgement and replay, and does not publish the new live owner before that boundary is established. This prevents gaps, duplication, or live output overtaking history.

When retained raw history is truncated, `first_sequence` and `omitted_prefix_bytes` report the exact omitted byte count. The retained suffix still ends at the watermark and remains byte-contiguous. A raw suffix can begin inside a terminal control sequence or fullscreen update, so truncation metadata is reported separately and is not injected into the terminal byte stream. Structured screen snapshots remain future work.

The client applies replay to its parser only after the complete transaction validates. A wrong lease, duplicate range, gap, overflow, mismatched watermark, or live output before `ReplayEnd` leaves the attachment unreconciled and requires a fresh attach replay.

## PTY and child lifecycle

Session IDs are reserved under the server lock before spawning PTYs, so duplicate requested IDs cannot race with creation. If spawning fails, the reservation is released. Fallible master-side setup occurs before spawn; an unpublished child is killed and definitively reaped before its handle is dropped.

Each published child has one waiter that exclusively owns its child handle and performs the successful reap. The PTY reader only records output and marks the stream drained. Both paths converge on one centralized lifecycle:

```text
Running -> Stopping -> Exited -> Removed
    \-----------------> Exited -> Removed
```

Finalization occurs only after the direct child is reaped **and** PTY output is drained. Under one exactly-once transition it removes the session, emits one definitive `PaneExited` to the current lease, and completes every pending correlated stop. Natural exit and manual stop cannot steal the child handle from each other. Interrupted or recoverable wait failures retain the sole handle and session for retry rather than fabricating an exit.

The client waits for attach confirmation and replay completion before considering a pane attached. Startup restoration uses attach-only and never sends `CreateSession`, so a missing persisted command cannot relaunch. Deletion waits for a correlated `StopResult`; rejected, timed-out, or disconnected stops leave local state intact pending reconciliation.

## Process-group termination and daemon shutdown

PTY children start as isolated session and process-group leaders. Stop and daemon shutdown use the same termination/finalization path:

1. reject further input, paste, resize, and detach for the stopping pane;
2. send `SIGTERM` to the stable child process group (and the current foreground group when distinct);
3. wait for a bounded grace period;
4. send `SIGKILL` if finalization did not complete;
5. wait for the sole waiter to reap the direct child and for PTY output to drain;
6. finalize and remove the session exactly once.

`SIGINT` or `SIGTERM` starts graceful daemon shutdown, blocks new session creation, attach/takeover, and mutations, and routes every existing pane through this path before removing the socket. Attach commits and shutdown are serialized under the daemon state lock: if shutdown wins, a new attach gets a cached correlated `AttachError::Failed` shutdown result, while an earlier cached success cannot rebind a replacement connection. A second termination signal uses the signal handler's forced-shutdown behavior. `SIGHUP` remains ignored so closing the launching terminal does not tear down sessions.

Portable process-group signaling covers the root PTY group plus a captured distinct foreground group. A descendant that deliberately creates a new session can escape those groups; fully supervising such descendants would require platform-specific process enumeration, cgroups/service supervision, or a launch wrapper.

## Delivery-uncertain terminal bytes

Raw input and paste deliberately have no request ID. They are **never automatically replayed**. If a socket write was attempted and then failed, the client returns a typed delivery-uncertain error: the daemon may have received none, part, or all of the frame, and retrying could duplicate terminal bytes. Resize and detach are also not silently replayed.

A serialization or size failure detected before the first frame write is an ordinary definite error. After uncertain delivery, the client keeps its local pane mapping/lease until takeover, a scoped rejection, a correlated stop/exit, or attach reconciliation proves otherwise. Users or higher layers may reconcile and then deliberately decide whether to send new input; the runtime does not make that decision automatically.

## Command execution semantics

`TerminalLaunch::Command`, `pi_agent_command`, and `claude_code_command` are user-configured command strings and intentionally run through the login shell as `$SHELL -lc <command>`. Pipelines, expansion, globbing, and shell quoting apply. This remains intentionally different from `MULT_AGENT_CMD`, which the client parses into argv without shell expansion.

## Operational notes

- Start manually with `just server` or `cargo run --bin mult-server`.
- Stop and restart the daemon after a protocol-version upgrade.
- Socket collisions with non-socket files are refused rather than removed.
