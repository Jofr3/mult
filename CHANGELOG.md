# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project aims to adhere to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Claude Code as a second chat-agent backend alongside `pi`. `Ctrl+x` (and the
  "New Claude Code chat" command-palette entry) opens a Claude Code agent chat,
  while `Ctrl+a` still opens a `pi` chat. The chosen backend is stored on the
  chat (`AgentKind`, defaulting to `pi` for pre-existing state) and shown in the
  sidebar as `agent: pi` / `agent: cc`. New `claude_code_command` and
  `auto_start_claude_code_agent` config options mirror the `pi` ones.
- Live sidebar status for Claude Code chats. `mult` generates a per-session
  Claude Code hooks file (passed via `--settings`, merged over the user's config
  without touching it on disk) whose `SessionStart` / `UserPromptSubmit` /
  `PreToolUse` / `Notification` / `Stop` events run a bundled script
  (`extensions/mult-claude-status.sh`) that writes the same status file `pi`'s
  extension does, so `cc` chats drive the status dot like `pi` chats.
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

- Mouse-wheel scrolling over a chat agent or terminal whose program has grabbed
  the mouse (Claude Code, `nvim`, `less`, …) is now forwarded to that program,
  encoded in the protocol it requested (SGR/UTF-8/X10). Previously the wheel was
  always applied to `mult`'s local scrollback, which is empty for an
  alternate-screen app, so Claude Code agent tabs could not be scrolled at all.
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
