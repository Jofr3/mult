# Contributing

Thanks for helping improve `mult`. The detailed contributor/agent guide lives
in [`AGENTS.md`](AGENTS.md) — please read it before making changes. This file
is the short version.

## Setup

```sh
nix develop          # cargo, clippy, rustfmt, just, cargo-deny, cargo-llvm-cov, cargo-fuzz
just run             # run the TUI client
just install-hooks   # optional: a pre-commit hook that runs cargo fmt --check
```

Without Nix, install `just` and `cargo-deny` (plus `cargo-llvm-cov` for
coverage and `cargo-fuzz` for the fuzz targets), then use the `cargo`/`just`
commands below. The pinned development toolchain lives in
[`rust-toolchain.toml`](rust-toolchain.toml).

**MSRV.** `Cargo.toml` declares `rust-version = "1.88"`, and CI has a job pinned
to exactly that version, so the claim is tested rather than asserted. The
development toolchain is deliberately newer (1.94); the MSRV job is what tells
you if a change needs something 1.88 does not have.

## Before opening a PR

Run the local gate:

```sh
just ci          # fmt-check, version-check, lint, test, cargo deny, extension typecheck
```

CI runs that same gate on Linux **and** macOS, plus separate jobs for the MSRV
check, `cargo deny`, coverage, the fuzz-target build, the extension typecheck and
`nix flake check`. For faster iteration:

```sh
cargo check --workspace --all-targets --all-features
cargo test  --workspace --all-targets --all-features
cargo fmt --all
```

There is deliberately no `just audit`: `cargo deny check` covers the RustSec
advisory database along with licences, banned crates and crate sources, and
`cargo audit` only ever re-checked a subset of it.

## Environment variables used by the tests

The integration suite drives real PTYs, which some sandboxes cannot allocate.
Two variables control it:

| Variable | Effect |
| --- | --- |
| `MULT_SKIP_PTY_INTEGRATION` | Set to any non-empty value to skip every PTY-backed test: `tests/pty_integration.rs`, and the `mult-server` dispatch tests that create a real pane. Use it when your environment cannot allocate a PTY — a Nix build sandbox has a `/dev/ptmx` symlink with no `devpts` behind it, so `openpty` fails with `ENOENT`. Those tests otherwise **fail loudly** rather than passing vacuously, and they are not fakeable: what they test is bytes moving across a pty master. |
| `MULT_TEST_SHELL` | Path to the shell the integration tests spawn. Set it where `/bin/sh` is absent or unusual, as it is inside a Nix build sandbox. |

Both are set by [`flake.nix`](flake.nix) for `nix build`, which runs in a sandbox
where neither a PTY nor `/bin/sh` is guaranteed.

The application's own variables (`MULT_CONFIG_PATH`, `MULT_STATE_PATH`,
`MULT_SOCKET_PATH`, `MULT_SERVER_AUTOSPAWN`, `NO_COLOR`) are documented in the
[README](README.md#environment-variables). Two more are set *by* `mult` for the
agent process it launches, rather than by you: `MULT_AGENT_STATUS_PATH` and
`MULT_AGENT_CHAT_ID`. When working on `extensions/`, note that both files are
compiled into the binary with `include_str!` (as the constants
`MULT_STATUS_EXTENSION_SOURCE` and `MULT_CLAUDE_STATUS_SCRIPT_SOURCE` in
`src/runtime/agent_launch.rs`) — there is no environment variable that points
them at a file on disk, so a change to either needs a rebuild.

## Coverage

```sh
just coverage        # summary, with a floor that fails on a sharp drop
just coverage-html   # browsable report under target/llvm-cov/html
```

The floor is a regression guard set below the current number, not a target. Do
not raise it as a side effect of unrelated work, and do not treat an uncovered
line as a bug on its own.

## Fuzzing

Two targets live in [`fuzz/`](fuzz), which is a **separate workspace** on
purpose: `libfuzzer-sys` needs nightly and a sanitizer runtime, and folding it
into the root workspace would drag it into every normal build, into `Cargo.lock`
and into `cargo deny`'s dependency graph.

| Target | What it covers |
| --- | --- |
| `protocol_read_message` | `mult_protocol::read_message` over an arbitrary byte stream, decoded as both `ClientMessage` and `ServerMessage`. Both ends of the socket parse bytes another process wrote. |
| `vt_response_detector` | The terminal-response state machine in `src/pty.rs` over arbitrary PTY output — a hand-written escape scanner with a fixed inline CSI buffer, fed bytes a PTY child chose. |

```sh
rustup toolchain install nightly     # cargo-fuzz needs a nightly rustc
nix develop
just fuzz-build                      # build both targets, no campaign

cd fuzz
cargo +nightly fuzz run protocol_read_message
cargo +nightly fuzz run vt_response_detector -- -max_total_time=300
```

Note the `cargo +nightly` form only works where `cargo` is the rustup proxy.
**Inside `nix develop` it is not** — nixpkgs' own `cargo` comes first on `PATH`
and rejects `+toolchain`. `just fuzz-build` handles this by resolving the
nightly binary itself; to run a campaign from the Nix shell, do the same:

```sh
PATH="$(dirname "$(rustup which --toolchain nightly cargo)"):$PATH" \
  cargo fuzz run vt_response_detector -- -max_total_time=300
```

A crash is written to `fuzz/artifacts/<target>/`; re-run that file to reproduce
it, and use `cargo fuzz tmin <target> <file>` to shrink it.

CI builds both targets and runs a 60-second smoke pass each. That is not fuzzing
— it catches a target that stopped compiling or that fails instantly. Real
campaigns are run by hand and are unbounded.

`cargo fuzz` builds with debug assertions and overflow checks on, so it surfaces
arithmetic panics that a release build would silently wrap. That is a feature,
but it means a finding needs checking against the release profile before you
describe its impact.

## Guidelines

- Keep changes narrow and buildable; preserve existing public behavior unless
  the change fixes a documented bug.
- Keep `Cargo.lock` in sync with dependency changes, and prefer not to add
  dependencies (see `AGENTS.md`).
- State files and runtime IPC are security-sensitive — see [`SECURITY.md`](SECURITY.md).
- Add a `CHANGELOG.md` entry under `[Unreleased]`.

## Where things are documented

- Tracked work and the execution plan: [`docs/BACKLOG.md`](docs/BACKLOG.md),
  [`docs/PLAN.md`](docs/PLAN.md)
- Daemon and socket design: [`docs/DAEMON.md`](docs/DAEMON.md)
- Every config key: [`docs/CONFIG.md`](docs/CONFIG.md)
- Failure modes and their exact messages: [`docs/TROUBLESHOOTING.md`](docs/TROUBLESHOOTING.md)
- Cutting a release: [`docs/RELEASING.md`](docs/RELEASING.md)
