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

The client and server exchange a protocol hello and require matching `PROTOCOL_VERSION`. A protocol mismatch usually means an old server is still running after an upgrade; stop it and start the new `mult-server` binary.

IPC messages are length-prefixed and encoded with `postcard`. Oversized frames are rejected before allocation.

## PTY lifecycle

Session IDs are reserved under the server lock before spawning PTYs, so duplicate requested IDs cannot race with creation. If spawning fails, the reservation is released. The client waits for attach confirmation and rolls back local attachment state if attach is rejected.

Client startup restores persisted running command terminals with an attach-only request. It does not send `CreateSession` on that path, so a missing or unreachable daemon session can never relaunch a persisted command. The client marks that terminal stopped/recoverable and requires deliberate user input before creating a replacement session. Deletion similarly waits for a pane-correlated `StopResult` before removing an attached PTY from client state; a rejected, timed-out, or disconnected stop leaves the item intact.

## Operational notes

- Start manually with `just server` or `cargo run --bin mult-server`.
- The server ignores SIGHUP so PTYs are not torn down when the launching terminal closes.
- Socket collisions with non-socket files are refused rather than removed.
