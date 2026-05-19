# AGENTS.md

Guidance for coding agents working on `mult`.

## Mission

Build `mult`: a Ratatui-based AI agent multiplexer with multiple workspaces, nested agent chats, and per-workspace terminals.

Do not jump straight to the full product. Keep changes milestone-sized and preserve a runnable minimal TUI at all times.

## Current milestone

M2 real terminal panes is in progress, but keep changes incremental:

- Keep the M1 durable model and open/import flow working.
- PTY runtime lives outside persisted project state.
- Terminal scrollback is in-memory only unless explicitly designed otherwise.
- Next small step: route keyboard input to the focused terminal pane.
- Preserve JSON persistence and do not save running terminal status as durable state.

See `docs/PLAN.md` for the roadmap.

## Development commands

Preferred workflow:

```sh
nix develop
just check
just test
just run
```

Useful commands:

```sh
just fmt        # format Rust; format Nix if nixpkgs-fmt exists
just lint       # clippy with warnings denied
just watch      # cargo-watch check/test loop
nix build       # verify flake package
nix flake check # run Nix checks
```

If `just` is unavailable outside the shell, use the underlying `cargo`/`nix` commands directly.

## Code style and architecture

- Keep Ratatui render functions pure: render from `&App`; no side effects in UI code.
- Keep user input/event handling outside render functions.
- Prefer small domain enums/structs over raw strings for statuses and navigation targets.
- Avoid coupling the UI directly to future PTY or agent backends; introduce traits/adapters when those milestones begin.
- Add tests for state transitions when changing navigation, workspace/session mutation, or status handling.
- Keep the app compiling and runnable after every change.

## Dependencies

- Runtime TUI: `ratatui` + `crossterm`.
- Nix dev shell should stay lightweight and fast.
- Do not add async runtimes, PTY crates, databases, or agent SDKs until the milestone needs them.

## Safety

- Do not run destructive filesystem commands.
- Do not store API keys, tokens, or credentials in the repo.
- Do not implement automatic agent command execution without explicit confirmation flows.
