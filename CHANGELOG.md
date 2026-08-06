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

- Modified special keys are now sent to the PTY using xterm's CSI modifier
  encoding (`CSI 1 ; <mod> <final>` / `CSI <n> ; <mod> ~`) instead of being
  blindly prefixed with ESC. `Alt`/`Ctrl`/`Shift` combined with the arrows,
  `Home`/`End`, `PageUp`/`PageDown`, `Insert`/`Delete`, or the function keys now
  reach the program correctly — e.g. `Alt+Left` sends `\x1b[1;3D` (move a word)
  rather than `\x1b\x1b[D`, which the program rendered as literal characters.
  `Ctrl+Arrow` and `Shift+Arrow`, which previously dropped their modifier, now
  carry it too.
- `Alt+Shift+<letter>` no longer loses its `Shift`. Under the Kitty disambiguate
  protocol the host reports the combination as the unshifted base key plus a
  separate `Shift` bit (e.g. `Alt+Shift+h` → `Char('h')` + `SHIFT|ALT`), and the
  character path dropped that bit — so `Alt+Shift+h/j/k/l` reached the PTY as
  `\x1bh`/`…` (`<M-h>`), indistinguishable from `Alt+h`. `Shift` is now folded
  back into the glyph, sending `\x1bH` (`<M-H>`) as a legacy app like `vim`
  expects.
- Unmodified arrows and `Home`/`End` follow the program's cursor-key mode
  (DECCKM): full-screen apps that request application cursor keys (`vim`, `less`,
  `fzf`, …) now receive the SS3 form (`\x1bOA`) they expect, while the shell keeps
  the normal CSI form (`\x1b[A`). This mirrors the existing per-program handling
  of bracketed paste and the mouse protocol.
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

### Fixed — daemon lock discipline and pane lifecycle

- `mult-server` no longer writes to a PTY while holding the global daemon lock.
  Client input and paste go through a bounded per-pane queue drained by a
  dedicated writer thread, so a child that stops reading its standard input can
  no longer freeze every pane and every client in the daemon. A full queue
  (1 MiB per pane) is **refused** with a pane-scoped `LeaseRejected` rather than
  silently dropped or blocked on; the connection stays usable.
- Attach replay releases the global daemon lock before it runs. It still holds
  the pane barrier that orders replay against live output, so an attach now
  serializes only the pane it attaches to instead of the whole daemon.
- An attach whose replay overflows the client's send queue no longer disconnects
  the client it has just attached; the attachment is left unreconciled and the
  client re-attaches, instead of looping through reconnects.
- Retained PTY history is stored as refcounted chunks instead of one flat
  buffer. Trimming is now O(bytes dropped) rather than an O(history) memmove per
  8 KiB read under the pane lock, and replay sends the pane's own chunks, so
  attaching no longer makes the whole retained history resident a second time
  (previously ~64 MiB per attaching client per pane).
- Retained history per pane is sized from the client's actual scrollback need
  (~2.4 MiB) instead of twice the wire frame limit (32 MiB).
- Daemon shutdown is bounded by a 10 s deadline. A pane whose stop driver timed
  out is never removed, and the unbounded wait for it left `mult-server` running
  forever with its socket still bound. The socket is now unlinked on every exit
  path, including an early startup failure.
- The PTY reader thread no longer probes `tcgetpgrp` and `/proc/<pid>/cmdline`
  after every 8 KiB read; it shares the debounced foreground-process poll the
  input path already used.
- Pane routing is a map lookup. The dead linear-scan fallback, which locked
  every pane while holding the daemon lock on every input, resize and detach,
  is gone.

### Fixed — render performance

- Terminal panes render without rebuilding a heap string for every cell on every
  frame. The vt100 adapter dropped a redundant clone of an already-owned
  `String`, stopped asking blank cells for contents they do not have, and stores
  each cell's text inline (24 bytes, which is exactly the six-codepoint maximum
  `vt100` can put in a cell) instead of on the heap. At 200×50 this measured
  755 → 578 µs and 15 189 → 2 585 allocations per frame, with a byte-identical
  rendered buffer.
- Terminal search no longer scrapes the whole screen into a `String` per row on
  every frame. The scrape is passed as a closure and runs only when a search is
  actually active — 42–46 µs and ~2 700 allocations per frame that were being
  discarded immediately.
- The colour scheme is parsed once instead of twelve hex parses per frame, and
  the Rosé Pine Moon defaults have a single definition again: `config.rs` holds
  the hex strings and the renderer derives its fallback colours from them at
  compile time, so the two can no longer disagree. A colour that fails to parse
  is now reported per key (still falling back to the default) rather than
  silently swallowed.
- Sidebar label truncation measures character widths without allocating.
- `PtyEvent::Output` / `Scrollback` carry a byte count instead of a copy of every
  chunk that crossed the socket; the bytes were already committed to the
  terminal's parser and no consumer read them. Adjacent chunks from the same pane
  are coalesced into one event per drain.
- The client's server-event queue holds 256 messages rather than 4096, capping
  the backlog at roughly 2 MiB instead of 32 MiB when the render thread stalls.

## [0.1.0]

Initial prototype: a Ratatui/Crossterm client plus a persistent `mult-server`
PTY daemon over a Unix socket — multiple workspaces with `pi` agent chats and
shell/command terminals, persistent JSON project state, terminal scrollback,
mouse selection, and OSC52 clipboard copy.
