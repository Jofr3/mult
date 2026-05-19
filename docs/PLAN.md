# mult plan

`mult` is an AI-agent multiplexer TUI. It should let one operator manage multiple workspaces, each with agent chats and dedicated terminals for dev servers, shells, tests, and one-off commands.

## Product shape

- **Workspace**: a project/repo/task context. Owns chats, terminals, cwd, environment, and persisted state.
- **Chat/session**: an agent conversation attached to a workspace. Shows status such as idle, thinking, waiting for input, failed, or done.
- **Terminal**: a PTY attached to a workspace. Used for dev servers, test runs, shells, logs, and commands not controlled by an agent.
- **Sidebar**: tree of open workspaces, with nested chats and terminals plus live status indicators.
- **Main pane**: selected chat transcript, terminal output, or workspace overview.
- **Future panes**: command palette, logs/events, diff/patch preview, notifications, and background job monitor.

## Milestones

### M0 — Shell prototype (done)

Goal: prove the app skeleton and UX layout.

- Rust binary using Ratatui and Crossterm.
- Nix flake dev shell with `cargo`, `rust-analyzer`, `clippy`, `rustfmt`, `just`, and `cargo-watch`.
- Static in-memory state for workspaces, chats, terminals, and status.
- Sidebar tree + detail pane + footer keybindings.
- Basic navigation and state mutation:
  - `j`/`k` or arrows: move selection
  - `w`: add workspace
  - `c`: add chat to selected workspace
  - `t`: add terminal to selected workspace
  - `r`: rotate selected chat/terminal status
  - `q`/Esc: quit

### M1 — Durable project model (done)

- [x] Move state into modules (`model`, `storage`, `ui`).
- [x] Add IDs instead of index-only references.
- [x] Persist open workspaces/sessions to a data directory.
- [x] Add workspace cwd/env metadata.
- [x] Add import/open workspace flow.

### M2 — Real terminal panes (in progress)

- [x] Add PTY abstraction via `portable-pty`.
- [x] Spawn shells with workspace cwd/env when a terminal is selected.
- [x] Stream terminal output into in-memory scrollback buffers.
- [ ] Handle keyboard input routing when a terminal pane is focused.
- [x] Track terminal process lifecycle and exit codes.
- [ ] Support resizing from pane dimensions instead of fixed 80x24.
- [ ] Spawn named commands/dev servers, not only default shells.

### M3 — Agent adapters

- Define an `AgentBackend` trait for sending prompts and streaming events.
- Support process-backed adapters first (for local CLIs), then HTTP/WebSocket adapters if useful.
- Normalize events: message delta, tool call, file change, command started, status changed, error.
- Keep agent sessions resumable per workspace.

### M4 — Layout and workflow UX

- Add focus modes: sidebar, chat, terminal, command palette.
- Support split panes/tabs per workspace.
- Add command palette for workspace/session/terminal actions.
- Add search/filter over chats and terminal scrollback.
- Add status bar and notification/event log.

### M5 — Safety and collaboration features

- Confirmation workflow for destructive agent actions.
- Patch/diff viewer before applying edits.
- Per-workspace policy config.
- Session export/import.
- Optional remote/headless daemon mode.

## Initial architecture notes

- Keep UI rendering immediate-mode and deterministic; derive it entirely from `App` state.
- Keep side effects outside render functions.
- Use channels/events for agent output, PTY output, and user input once async work starts.
- Prefer small domain types over strings for status, IDs, and commands.
- Add integration tests around state reducers before adding real processes.
