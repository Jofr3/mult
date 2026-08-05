# Contributor / agent guide

This repository is a Rust workspace for `mult`, a terminal UI plus a small persistent PTY daemon.

## Before editing

- Read `README.md` for user-facing behavior and controls.
- For daemon/socket behavior, read `docs/DAEMON.md`.
- For config keys, read `docs/CONFIG.md`; for the messages the code produces on failure, `docs/TROUBLESHOOTING.md`. Both are kept verified against the source — if you change a message or a key, update them in the same change.
- Keep changes narrow and buildable; do not rewrite large subsystems unless the task explicitly requires it.
- Preserve existing public behavior unless the change fixes a documented bug or improves documented behavior.

## Validation

Use the strict local gate when possible:

```sh
nix develop
just ci
```

`just ci` runs formatting checks, the release-version consistency check, clippy with `-D warnings`, tests, `cargo deny check` and the extension typecheck. If you are outside the Nix shell, install `just` and `cargo-deny` first. (`cargo audit` used to run alongside `cargo deny` and only re-checked a subset of its advisories section; it was dropped.)

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
- `state.json` is an **execution boundary**, not just a record: it stores command lines that are run through the login shell, plus the workspace `cwd` and `environment` every terminal inherits, and it lives at an `$MULT_STATE_PATH`-overridable path. Treat its contents as untrusted input. Nothing it names may be executed at startup without the user having been shown it and having agreed — shell terminals restore automatically (their program comes from `$SHELL`), command terminals go through the startup confirmation prompt. It also holds the daemon `instance` token that namespaces this client's sessions; do not regenerate it on load, or a restart abandons its own live panes.
- `pi_agent_command` and `claude_code_command` (the chat-agent backends, selected per chat by `AgentKind`) and `TerminalLaunch::Command` are run through the login shell (`$SHELL -lc <command>`), so they *are* shell-evaluated — pipelines, `$VAR` expansion, and globbing all apply. This is the user's own config, not a privilege boundary. (An `MULT_AGENT_CMD` argv-split path used to exist alongside it; Slice 11a deleted it as unreachable — see F1.)
