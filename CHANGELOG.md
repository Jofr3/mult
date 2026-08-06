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

### Changed — CI, documentation and release

- **One roadmap.** `README.md` and `CONTRIBUTING.md` pointed at
  `docs/REMAINING_WORK.md` as the authoritative follow-up list while the list
  work was actually being done from was linked from nowhere. There is now a
  single entry point, `docs/ROADMAP.md`, fronting `docs/BACKLOG.md` (items) and
  `docs/PLAN.md` (execution order). Nothing was discarded: the Phase 3
  transcript contract, the open decisions about incomplete public paths, the
  extension-dependency migration, the standing per-phase rules and the Phase 7
  projects were carried into it, and `REMAINING_WORK.md`, `BACKLOG-v1.md` and
  `PLAN-v1.md` are retained with explicit historical banners.
- **The MSRV job now provably tests 1.88.** It ran a bare `cargo check` while
  `rust-toolchain.toml` pins 1.94, and a toolchain file outranks
  `rustup default` — so whether it tested the MSRV at all depended on the
  toolchain action exporting `RUSTUP_TOOLCHAIN`. It runs `cargo +1.88` now.
  1.88 was re-verified against the current tree.
- **`just ci` matches CI again.** It was `fmt-check lint test audit` while CI
  additionally ran macOS, `cargo deny`, an npm typecheck and more. It is now
  `version-check fmt-check lint test deny typecheck`, and GitHub Actions runs
  exactly that on Linux and macOS. The extension typecheck degrades to a notice
  when `npm` or `extensions/node_modules` is missing, so the gate still
  completes offline.
- **`cargo audit` removed in favour of `cargo deny`.** Both ran, and `deny.toml`
  already said deny supersedes audit. `just audit` is now `just deny`, and the
  redundant standalone CI job is gone since `just ci` covers it.
- **CI runs weekly and on demand**, not only on push and pull request, so a
  newly published RustSec advisory surfaces without anyone opening a PR.
- **Coverage is measured.** `just coverage` and a CI job run `cargo llvm-cov`.
  The baseline is 82.13% of lines (18 150 lines, 3 243 uncovered); CI's floor is
  75%, deliberately well below it so unrelated work is not blocked.
- **Tag-triggered releases.** `.github/workflows/release.yml` builds Linux
  (gnu + musl) and macOS (x86_64 + aarch64) archives, each containing **both**
  binaries plus both licences — the client resolves the daemon from the path
  next to its own executable, so they have to ship together. The tag is checked
  against the declared crate version and the full gate runs *before* anything is
  published; the release stays a draft until every target has uploaded.
  `docs/RELEASING.md` documents the process. No tag has been cut.
- **The version is declared once.** `[workspace.package] version` is inherited
  by `crates/protocol`; `flake.nix` and `extensions/package.json` still mirror it
  and `just version-check` (wired into `just ci`) fails if they disagree.
- **`cargo-deny` is in the dev shell**, which `CONTRIBUTING.md` had promised for
  some time without it being true. `cargo-llvm-cov` joined it.
- **New documentation.** `docs/CONFIG.md` covers all 12 colorscheme keys and
  every top-level key with type, default and effect — the README documented 3 of
  12, and `_nc` was unguessable. `docs/TROUBLESHOOTING.md` maps the failure
  modes this code actually produces to fixes, quoting the messages from source.
- **The README project layout was regenerated from the tree** — it omitted
  `runtime.rs`, `git.rs`, `paths.rs`, `lib.rs`, `terminal_guard.rs` and
  `transcript.rs`, and attributed the event loop to `main.rs`. The environment
  table now lists every `MULT_*` variable the code reads, marks `MULT_AGENT_CMD`
  experimental and inert rather than implying it works, and separates the
  variables `mult` sets for its children from the ones you set.
  `extensions/package.json` no longer claims the extension is embedded in
  `src/main.rs` (it is `src/runtime.rs`).
- **Issue and PR templates, and `CODEOWNERS`.** The bug template captures
  terminal emulator, `$TERM`, OS and whether the kitty keyboard protocol is
  active, since input encoding depends on all four.
- **`CHANGELOG.md` reference links.** `[Unreleased]` and `[0.1.0]` rendered
  literally for want of link definitions; `[0.1.0]` is now dated.
- **`.gitignore`** covers `.claude/settings.local.json`, `*.corrupt-*`
  (state backups), `src/snapshots/*.snap.new` (insta), the future `fuzz/`
  outputs, and editor/OS scratch.
- **`just install-hooks`** writes a pre-commit hook that runs only
  `cargo fmt --all -- --check` — fast enough not to be worth bypassing.

### Security — hardening slice R4

- **Socket peer verification now fails closed on every platform.** `peer_uid`
  returned "unknown" on every non-Linux target and both callers treated that as
  *accept*, so on macOS/BSD a squatted socket passed validation and could read
  every keystroke in every pane and inject input. There is now a single shared
  implementation (`mult_protocol::peer`) using `SO_PEERCRED` on Linux/Android
  and `getpeereid(3)` on macOS and the BSDs; a credential that cannot be
  obtained — including on a platform with no such API — is a hard rejection.
  The byte-duplicated check in `pty.rs`/`mult-server.rs` and the three copies of
  `current_euid` (plus the inline `geteuid()` calls in `storage.rs`,
  `paths.rs` and the protocol crate) are gone.
- **`config.json` is read with the same discipline as state.** It was a bare
  `fs::read`: symlink-following, unbounded, with no owner, mode or regular-file
  check — while the `pi_agent_command`/`claude_code_command` it yields are
  shell-evaluated and auto-started by default, making a planted symlink or write
  at that path silent code execution with no keystroke required. Both
  attacker-steerable routes to it (`$MULT_CONFIG_PATH` and `$XDG_CONFIG_HOME`)
  are now closed by the same check: every parent component is opened with
  `O_NOFOLLOW`, the containing directory must be owned by this user and not
  group/other-writable, and the file must be a regular, singly-linked,
  owner-only file read under a 1 MiB cap. State and config share one hardened
  read implementation. **This breaks a symlinked `config.json`**, which is what
  dotfile managers such as GNU stow leave behind; copy the file or point
  `$MULT_CONFIG_PATH` at a real one. `docs/TROUBLESHOOTING.md` has the messages
  and the workaround.
- **The state read is size-capped** (16 MiB). A large *regular* state file — one
  passing every ownership and link check — could still OOM the client at
  startup.
- **Private files are proved owner-only before their bytes are read.** The
  mode is re-checked after the normalizing `fchmod`, closing the inconsistency
  with `read_mult_agent_status_records`, which already refused `mode & 0o077`.
- **The git branch probe no longer runs `git`.** It forked `git -C <cwd>` every
  two seconds per workspace with no `GIT_CONFIG_NOSYSTEM`, ceiling directory or
  `--git-dir` pin, so a hostile repository's `.git/config` (`include.path`,
  `core.fsmonitor`, `core.hooksPath`) was parsed merely by opening the
  workspace. The branch is now read from the first line of `.git/HEAD` with a
  bounded, `O_NOFOLLOW`, regular-file-checked read that follows `gitdir:`
  pointers and resolves a symlinked `.git` deliberately — a broken link yields
  no branch rather than the enclosing repository's. Detached `HEAD` and non-repo
  directories behave as before, and a branch name containing control characters
  is rejected instead of being rendered.
- **Daemon autospawn checks the binary and clears the environment.** The
  daemon was resolved purely by filename next to `current_exe()` with no owner
  or mode check, and inherited the first client's entire environment — which it
  then handed to every later client's PTYs, re-exporting one shell's API keys
  into every pane. It is now executed only when it (and its directory) is owned
  by this user or root and not group/other-writable, and is spawned with
  `env_clear()` plus an allow-list (`PATH`, `HOME`, `SHELL`, `USER`, `LOGNAME`,
  `TERM`, `LANG`, `LC_*`, `MULT_*`).
- **Terminal query auto-responses are coalesced and bounded.** They accumulated
  in an uncapped `Vec<Vec<u8>>` and were sent as one blocking socket write each
  from the render thread — roughly 2048 writes per 8 KiB of `\x1b[6n`. A chunk
  of output now produces at most one cursor report, at most eight answers, and
  exactly one write.
- **Daemon-supplied text can no longer forge terminal output.** `PtyEvent::Error`
  messages and `ExitInfo::signal` names reached `parser.process()` raw; control
  bytes in a `[mult]` system line are now replaced with U+FFFD.
- **`TranscriptJournal::open` is no longer destructive.** It called `set_len()`
  on any caller-supplied file whose tail lacked a newline, and `fchmod`ed the
  parent directory to `0700` — so a mistyped path silently truncated a file and
  re-permissioned a directory. Recovery is now opt-in
  (`TranscriptRecovery::TruncatePartialTail`) and opening never changes an
  existing parent's mode.
- **The status-bridge extensions stop following symlinks when appending.**
  `mult-claude-status.sh` refuses a symlinked journal, and `mult-status.ts` uses
  `lstat` plus `O_NOFOLLOW` instead of `statSync` + `appendFileSync`.

### Changed — hardening slice R4

- OSC 52 clipboard writes are queued and emitted through the frame's own output
  after the next draw instead of being written straight to `io::stdout()` from a
  mouse handler, they are wrapped in tmux's passthrough DCS when `$TMUX` is set
  (previously copying inside tmux silently did nothing), and they can be turned
  off with the new `clipboard_osc52` config key (default `true`, today's
  behaviour).

## [0.1.0] - 2026-05-19

Initial prototype: a Ratatui/Crossterm client plus a persistent `mult-server`
PTY daemon over a Unix socket — multiple workspaces with `pi` agent chats and
shell/command terminals, persistent JSON project state, terminal scrollback,
mouse selection, and OSC52 clipboard copy.

> Dated from the first commit. **No `v0.1.0` tag has been cut and no binaries
> were published**, so the link below resolves only once the tag exists. See
> [docs/RELEASING.md](docs/RELEASING.md) for the process.

[Unreleased]: https://github.com/Jofr3/mult/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Jofr3/mult/releases/tag/v0.1.0
