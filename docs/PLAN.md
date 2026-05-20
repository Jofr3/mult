# mult plan

`mult` is an AI-agent multiplexer TUI. It lets one operator keep several project workspaces open, run agent chats, and keep per-workspace terminals/dev commands nearby.

## Current implementation snapshot

- Ratatui/Crossterm TUI with a sidebar tree and a selected main pane.
- Durable JSON project state at `$XDG_DATA_HOME/mult/state.json` or `~/.local/share/mult/state.json`.
- Workspaces have stable IDs, optional cwd, environment metadata, agent chats, and terminal definitions.
- Chat transcripts, terminal definitions, and restorable running chat/terminal status are persisted; PTY process memory and terminal output are not persisted.
- PTYs are managed in-memory with `portable-pty`, visible-pane resizing, in-memory scrollback, shell terminals, and command/dev-server terminals.
- Selected chat panes can run an embedded `pi` process through a PTY. Config is loaded from `$XDG_CONFIG_HOME/mult/config.json` or `~/.config/mult/config.json`.
- The default UI palette is Rosé Pine Moon, with per-color `colorscheme` config overrides.
- Workspace/chat/terminal deletion uses a two-key `d d` chord and stops related running PTYs.
- Normal mode covers workspace/chat/terminal management; input mode is only for typing into the selected terminal or pi agent.
- Focus is explicit for sidebar, chat pane, and terminal pane. `Enter` moves from the sidebar into the selected pane, `Esc` returns to the sidebar, and the unfocused pane uses a darker background.

## Product shape

- **Workspace**: a project/repo/task context. Owns chats, terminals, cwd, environment, and persisted state.
- **Chat/session**: an agent conversation attached to a workspace. Shows status such as idle, thinking, waiting for input, failed, or done. Current operational path is an embedded pi PTY; the adapter trait remains groundwork for future backends.
- **Terminal**: a PTY attached to a workspace. Used for dev servers, test runs, shells, logs, and commands not controlled by an agent.
- **Sidebar**: tree of open workspaces, with nested chats and terminals plus live status indicators.
- **Main pane**: selected chat transcript/pi PTY, terminal screen, or workspace overview.
- **Future panes**: command palette, diff/patch preview, and background job monitor.

## Milestones

### M0 — Shell prototype (done)

Goal: prove the app skeleton and UX layout.

- [x] Rust binary using Ratatui and Crossterm.
- [x] Nix flake dev shell with `cargo`, `rust-analyzer`, `clippy`, `rustfmt`, `just`, and `cargo-watch`.
- [x] Static in-memory seed state for workspaces, chats, terminals, and status.
- [x] Sidebar tree + detail pane + footer keybindings.
- [x] Basic navigation and mutation.

### M1 — Durable project model (done)

- [x] Move state into modules (`model`, `storage`, `ui`).
- [x] Add stable IDs instead of index-only references.
- [x] Persist open workspaces/sessions to a data directory.
- [x] Add workspace cwd/env metadata.
- [x] Add import/open workspace flow.
- [x] Persist restorable chat/terminal running status without persisting PTY process memory.

### M2 — Real terminal panes (done for the current terminal model)

- [x] Add PTY abstraction via `portable-pty`.
- [x] Spawn shells with workspace cwd/env.
- [x] Spawn named command/dev-server terminals.
- [x] Stream PTY output into in-memory terminal screen buffers.
- [x] Handle keyboard input routing when a terminal pane is in PTY input mode.
- [x] Track terminal process lifecycle and exit codes.
- [x] Support resizing from visible pane dimensions instead of fixed 80x24.

Notes for future M4/M5 work: terminal buffers keep in-memory scrollback for paging, but do not persist it. Search/filter remains future work.

### M3 — Agent runtime groundwork (done for now)

- [x] Define an `AgentBackend` trait for prompt/event backends.
- [x] Add a process-backed adapter scaffold and normalized events: message delta, tool call, file change, command started, status changed, and error.
- [x] Persist chat messages produced by normalized events.
- [x] Keep actual running chat UI decoupled by using embedded pi PTYs for the current product path.

Future adapter work: add a prompt composer, backend selection per workspace/chat, resumable backend metadata, and explicit confirmation around any agent-launched external process or destructive action.

### M4 — Layout and workflow UX (current milestone)

Completed in this milestone:

- [x] Render a pi agent directly in selected chat panes via PTY.
- [x] Load user config from `~/.config/mult/config.json`, including colorscheme overrides.
- [x] Delete workspaces, agent chats, and terminals with a two-key `d d` chord.
- [x] Add explicit focus modes for sidebar/chat/terminal without changing PTY backend behavior.
- [x] Use borderless panes with darker backgrounds for unfocused panes.
- [x] Keep sidebar navigation scoped to sidebar focus.
- [x] Collapse user-visible modes to normal mode and input mode.
- [x] Restart persisted running chat agents and terminals on app start.
- [x] Add in-memory terminal scrollback paging for chat-agent, command, and shell panes.

Remaining M4 workflow polish, to do incrementally:

- [ ] Add a command palette for workspace/session/terminal actions.
- [ ] Add search/filter over terminal output and chat transcripts.
- [ ] Add split panes/tabs per workspace after a pane layout model exists.

### M5 — Safety and collaboration features (future)

- [ ] Confirmation workflow for destructive agent actions.
- [ ] Patch/diff viewer before applying edits.
- [ ] Per-workspace policy config.
- [ ] Session export/import.
- [ ] Optional remote/headless daemon mode.

## Architecture notes

- Keep UI rendering immediate-mode and deterministic; derive it entirely from `App` state.
- Keep side effects outside render functions.
- Keep PTY runtime state outside persisted project state.
- Use channels/events for agent output, PTY output, and user input boundaries.
- Prefer small domain types over strings for status, IDs, focus, and commands.
- Add tests around state reducers when changing navigation, focus, workspace/session mutation, or status handling.
