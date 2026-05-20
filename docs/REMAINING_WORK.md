# Remaining work

This document captures follow-up work after the protocol, PTY lifecycle, storage, and UI hardening pass.

## Highest priority

1. **End-to-end PTY integration tests**
   - Start `mult-server` on an isolated socket path.
   - Connect through the client runtime.
   - Spawn a short-lived shell/command terminal.
   - Verify snapshot/update delivery and real exit reporting.
   - Gate tests so they are skipped when the environment cannot provide a PTY reliably.

2. **Rapid lifecycle testing**
   - Cover quick start/stop/restart flows for terminals and embedded chat agents.
   - Verify client and server registries stay consistent after stop failures, natural exits, and reconnects.

3. **Terminal parser coverage**
   - Add more focused tests for escape sequences, including alternate screen behavior, scroll regions, erase modes, SGR resets, tabs, and cursor save/restore.

## M4 workflow polish

1. **Command palette**
   - Add a small command palette for workspace, chat, terminal, and focus actions.
   - Keep command execution routed through existing app state transitions.

2. **Search/filter**
   - Add search over in-memory terminal scrollback and persisted chat transcripts.
   - Keep terminal scrollback in memory only unless a persistence design is added explicitly.

3. **Pane layout model**
   - Design a small pane layout abstraction before adding split panes or tabs.
   - Preserve pure Ratatui rendering from `&App`.

## Security and robustness follow-ups

1. **Socket ownership/peer validation**
   - Consider UID-based socket path fallback and/or Unix peer credential validation.
   - Keep socket permissions owner-only.

2. **State recovery strategy**
   - Define behavior for invalid or corrupt state JSON.
   - Options include fail-fast with a clear error, backup-and-reset, or explicit recovery prompt.

3. **Dependency audit tooling**
   - Consider adding `cargo-audit` to the dev shell if acceptable for dependency footprint.
   - `just audit` already skips gracefully when the tool is unavailable.

## Documentation and release hygiene

1. Keep `README.md`, `docs/PLAN.md`, and `docs/DAEMON.md` aligned as daemon/protocol behavior evolves.
2. Before release, run:
   - `cargo fmt -- --check`
   - `just check`
   - `just test`
   - `just lint`
   - `nix flake check`
