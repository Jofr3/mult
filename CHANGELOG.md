# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project aims to adhere to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Minimum supported Rust version declared on both crates (`rust-version = "1.88"`)
  and the toolchain pinned via `rust-toolchain.toml` so CI and contributors align.
- Release build profile: thin LTO, a single codegen unit, and stripped symbols
  (`panic = "unwind"` kept on purpose for daemon/client cleanup).
- `cargo-deny` configuration and a CI job covering advisories, licenses, bans,
  and crate sources.
- macOS added to the CI matrix; Dependabot for Cargo and GitHub Actions; a
  `tsc --noEmit` typecheck for the bundled `pi` status extension.
- `SECURITY.md`, `CONTRIBUTING.md`, and this changelog.

### Changed

- CI installs `just` / `cargo-audit` / `cargo-deny` from prebuilt binaries
  instead of compiling them from source.

### Fixed

- Completed the truncated `LICENSE-APACHE` (added the standard appendix).

### Security

- The `/tmp` socket fallback (used when `XDG_RUNTIME_DIR` is unset) is keyed on
  `geteuid()` instead of the spoofable `$USER`/`$UID`, and the socket and runtime
  directories are ownership-verified — rejecting pre-created ("squatted"),
  symlinked, or group/other-writable paths — before use.
- The agent status file, read once per frame per chat, is now opened with
  `O_NOFOLLOW`/`O_NONBLOCK`, checked to be a regular file, and read with a 64 KiB
  cap, so a hostile or buggy same-UID writer cannot stall or OOM the UI thread.
- The corrupt-state backup uses an unpredictable, atomically-renamed name instead
  of an `exists()`-then-rename probe.
- Documented that `pi_agent_command` (and `TerminalLaunch::Command`) are run
  through the login shell (`$SHELL -lc`) and are therefore shell-evaluated,
  unlike the argv-split `MULT_AGENT_CMD`.

## [0.1.0]

Initial prototype: a Ratatui/Crossterm client plus a persistent `mult-server`
PTY daemon over a Unix socket — multiple workspaces with `pi` agent chats and
shell/command terminals, persistent JSON project state, terminal scrollback,
mouse selection, and OSC52 clipboard copy.
