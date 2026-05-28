# Contributor / agent guide

This repository is a Rust workspace for `mult`, a terminal UI plus a small persistent PTY daemon.

## Before editing

- Read `README.md` for user-facing behavior and controls.
- For daemon/socket behavior, read `docs/DAEMON.md`.
- Keep changes narrow and buildable; do not rewrite large subsystems unless the task explicitly requires it.
- Preserve existing public behavior unless the change fixes a documented bug or improves documented behavior.

## Validation

Use the strict local gate when possible:

```sh
nix develop
just ci
```

`just ci` runs formatting checks, clippy with `-D warnings`, tests, and `cargo audit -D warnings`. If you are outside the Nix shell, install `just` and `cargo-audit` first.

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
