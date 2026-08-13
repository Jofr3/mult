# Contributor / agent guide

This repository is a Rust workspace for `mult`, a terminal UI plus a small persistent PTY daemon.

## Before editing

- Read `README.md` for user-facing behavior and controls.
- For planned work, start at `docs/ROADMAP.md` (it fronts `docs/BACKLOG.md` and `docs/PLAN.md`). Refer to work by its backlog ID.
- For daemon/socket behavior, read `docs/DAEMON.md`.
- For config keys, `docs/CONFIG.md`; for failure modes, `docs/TROUBLESHOOTING.md`.
- Keep changes narrow and buildable; do not rewrite large subsystems unless the task explicitly requires it.
- Preserve existing public behavior unless the change fixes a documented bug or improves documented behavior.

## Validation

Use the strict local gate when possible:

```sh
nix develop
just ci
```

`just ci` runs `version-check`, formatting checks, clippy with `-D warnings`, tests, `cargo deny check` (advisories/licenses/bans/sources — it supersedes the standalone `cargo audit`, which no longer runs), and a `tsc --noEmit` typecheck of the bundled status extension that skips with a notice when npm is unavailable. If you are outside the Nix shell, install `just` and `cargo-deny` first.

For smaller iterations, prefer:

```sh
cargo check --workspace --all-targets --all-features
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Editing notes

- Run `cargo fmt --all` before final validation.
- Keep `Cargo.lock` in sync with dependency changes.
- Do not add dependencies unless they clearly reduce risk or replace an unsafe/unmaintained dependency.
- State files and runtime IPC are security-sensitive; keep paths private and avoid predictable public `/tmp` files.
- `MULT_AGENT_CMD` is parsed by `mult`, not a shell: basic quotes and backslash escapes are supported, but shell expansion is intentionally not.
- `pi_agent_command` and `claude_code_command` (the chat-agent backends, selected per chat by `AgentKind`), `file_manager_command` (`Ctrl+n`), `editor_command` (`Ctrl+e`, resolved from `$VISUAL`/`$EDITOR` when unset) and `TerminalLaunch::Command` are the opposite: they are run through the login shell (`$SHELL -lc <command>`), so they *are* shell-evaluated — pipelines, `$VAR` expansion, and globbing all apply. This is the user's own config, not a privilege boundary, but the two agent-launch paths deliberately have different semantics, so keep them distinct.
