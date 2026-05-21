# Remaining work

The implementation items previously tracked here are complete for the current milestone.

## Completed in this pass

- Added end-to-end PTY integration tests that start `mult-server` on an isolated socket, connect with the client runtime, verify snapshot/update delivery, observe real exit status, and cover rapid stop/restart plus chat-agent runtime IDs.
- Added lifecycle registry coverage for pane exits and disconnected receivers/reconnect preparation, and fixed stale exit delivery after explicit stop by taking the stopped child out of the server pane before removing it.
- Expanded terminal parser coverage and behavior for alternate screen restore, scroll regions, erase modes, SGR resets, tabs, and cursor save/restore.
- Added a command palette for workspace, chat, terminal, focus, search, and quit actions.
- Added search/filter UI state for selected terminal output and persisted chat transcripts.
- Added a small pane layout model in the Ratatui layer as groundwork for future split panes/tabs.
- Added `MULT_SOCKET_PATH` for isolated/user-selected sockets while keeping owner-only socket permissions and the UID/USER fallback path.
- Added corrupt state JSON recovery by backing up the bad file to `*.corrupt-*` and resetting to default state.
- Added `cargo-audit` to the Nix dev shell while keeping `just audit` graceful when unavailable.
- Updated README, PLAN, and daemon documentation for the new controls, socket behavior, and recovery behavior.

## Future release checks

Before release, run:

- `cargo fmt -- --check`
- `just check`
- `just test`
- `just lint`
- `nix flake check`
