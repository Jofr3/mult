# Contributing

Thanks for helping improve `mult`. The detailed contributor/agent guide lives
in [`AGENTS.md`](AGENTS.md) — please read it before making changes. This file
is the short version.

## Setup

```sh
nix develop      # provides cargo, clippy, rustfmt, just, cargo-audit, cargo-deny
just run         # run the TUI client
```

Without Nix, install `just`, `cargo-audit`, and `cargo-deny`, then use the
`cargo`/`just` commands below. The pinned toolchain lives in
[`rust-toolchain.toml`](rust-toolchain.toml).

## Before opening a PR

Run the local gate:

```sh
just ci          # fmt-check, clippy -D warnings, tests, cargo audit
```

CI additionally runs `cargo deny` (advisories/licenses/bans/sources) and a
`nix flake check`. For faster iteration:

```sh
cargo check --workspace --all-targets --all-features
cargo test  --workspace --all-targets --all-features
cargo fmt --all
```

Guidelines:

- Keep changes narrow and buildable; preserve existing public behavior unless
  the change fixes a documented bug.
- Keep `Cargo.lock` in sync with dependency changes, and prefer not to add
  dependencies (see `AGENTS.md`).
- State files and runtime IPC are security-sensitive — see [`SECURITY.md`](SECURITY.md).

Known follow-ups are tracked in
[`docs/REMAINING_WORK.md`](docs/REMAINING_WORK.md); the daemon/socket design is
in [`docs/DAEMON.md`](docs/DAEMON.md).
