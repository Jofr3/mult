# Contributing

Thanks for helping improve `mult`. The detailed contributor/agent guide lives
in [`AGENTS.md`](AGENTS.md) — please read it before making changes. This file
is the short version.

## Setup

```sh
nix develop      # cargo, clippy, rustfmt, rust-analyzer, just, cargo-deny, cargo-llvm-cov, cargo-watch
just run         # run the TUI client
just install-hooks  # optional: pre-commit hook running `cargo fmt --all -- --check`
```

Without Nix, install `just` and `cargo-deny` (and `cargo-llvm-cov` if you want
coverage), then use the `cargo`/`just` commands below. The pinned toolchain
lives in [`rust-toolchain.toml`](rust-toolchain.toml).

## Before opening a PR

Run the local gate:

```sh
just ci
```

`just ci` is `version-check`, `fmt-check`, `lint` (clippy `-D warnings`),
`test`, `deny` (`cargo deny check`) and `typecheck` (`tsc --noEmit` on the
bundled status extension). `deny` fails if `cargo-deny` is missing; `typecheck`
skips with a notice when `npm` or `extensions/node_modules` is unavailable, so
`just ci` still runs offline.

That is the same set GitHub Actions runs, on Linux **and** macOS. CI adds four
jobs on top: the Rust 1.88 MSRV check (`cargo +1.88 check …`), `cargo llvm-cov`
coverage, a real `npm ci` before the extension typecheck, and `nix flake check`.
CI also runs weekly on a schedule so a new advisory is caught without a push.

Worth running locally before anything release-shaped:

```sh
cargo +1.88 check --workspace --locked --all-targets --all-features
just coverage
nix flake check
```

For faster iteration:

```sh
cargo check --workspace --all-targets --all-features
cargo test  --workspace --all-targets --all-features
cargo fmt --all
```

## Fuzzing

The `fuzz/` crate holds two `cargo-fuzz` targets. It is **its own workspace**, so
nothing above touches it: normal builds, `cargo deny` and the MSRV job never see
`libfuzzer-sys`, and `just ci` does not run it. It needs a nightly toolchain.

```sh
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz build
cargo +nightly fuzz run protocol_read_message -- -max_total_time=60
cargo +nightly fuzz run vt_response_detector  -- -max_total_time=60
```

| Target | What it drives |
| --- | --- |
| `protocol_read_message` | An arbitrary byte stream through `mult_protocol::read_message` in both directions. Any error is fine; a panic, an unbounded allocation or a hang is not. |
| `vt_response_detector` | Arbitrary PTY output through the terminal-query responder and the emulator, with interleaved resizes, at pane sizes taken from the input — including the clamped floor. This is the target that found `A13`. |

`fuzz/target`, `fuzz/corpus` and `fuzz/artifacts` are gitignored. A crash lands
in `fuzz/artifacts/<target>/`; reproduce it with `cargo +nightly fuzz run
<target> <artifact>`, then add the shrunk input as a deterministic regression
test in the crate it belongs to rather than relying on the corpus.

`fuzz_feed_terminal_output` in `src/pty.rs` is the seam the second target uses;
it is behind the `fuzzing` feature and is not compiled into anything shipped.

## Test environment variables

Two variables gate the PTY integration suite. Both are read by
`tests/pty_integration.rs` and are set by `flake.nix` for the sandboxed Nix
build.

| Variable | Effect |
| --- | --- |
| `MULT_SKIP_PTY_INTEGRATION` | Set to `1` to skip the whole PTY integration suite. Set by `flake.nix` because a Nix build sandbox cannot reliably allocate interactive PTY devices. **Setting it locally makes the suite report green while testing nothing** — do not leave it set in a shell you validate changes from. |
| `MULT_TEST_SHELL` | The shell the integration tests spawn panes with. Set by `flake.nix` to `runtimeShell`. Note that the *production* code paths read `$SHELL`, not this — so a sandbox that sets only `MULT_TEST_SHELL` still has no shell for anything outside that file. |

If you add a test that spawns a pane, check which of the two applies before
assuming the Nix build covers it.

## Guidelines

- Keep changes narrow and buildable; preserve existing public behavior unless
  the change fixes a documented bug.
- Keep `Cargo.lock` in sync with dependency changes, and prefer not to add
  dependencies (see `AGENTS.md`).
- Add a deterministic regression test for every fixed race or failure path.
- Wire-protocol changes bump `PROTOCOL_VERSION` and change client and daemon
  together. Durable-state changes bump `STATE_VERSION` with an explicit
  migration and a golden fixture.
- The release version is declared once in `[workspace.package]` and mirrored in
  `flake.nix` and `extensions/package.json`; `just version-check` guards it.
- State files and runtime IPC are security-sensitive — see [`SECURITY.md`](SECURITY.md).

## Where work is tracked

Start at **[`docs/ROADMAP.md`](docs/ROADMAP.md)**. It is the single entry point
for the item list ([`docs/BACKLOG.md`](docs/BACKLOG.md)), the execution order
([`docs/PLAN.md`](docs/PLAN.md)), the standing rules, and the open design
decisions that are not yet items. Referring to work by its backlog ID (`C7`,
`H9`, …) in an issue, commit or PR is the most useful thing you can do.

Also: the daemon/socket design is in [`docs/DAEMON.md`](docs/DAEMON.md), config
keys in [`docs/CONFIG.md`](docs/CONFIG.md), failure modes in
[`docs/TROUBLESHOOTING.md`](docs/TROUBLESHOOTING.md), and the release process in
[`docs/RELEASING.md`](docs/RELEASING.md).
