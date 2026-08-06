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

### Fixed — client responsiveness (R2)

- **A dead or slow daemon no longer freezes the UI.** Re-attaching after a
  reconnect used to walk every terminal serially on the render thread, each with
  its own 2 s attach timeout, on *every* frame that retried — roughly 2N seconds
  of frozen UI per frame with N terminals. Re-attachments are now queued and
  serviced with a 100 ms per-frame budget (so a wedged daemon costs one stalled
  round trip per frame instead of N), and failed reconnects back off from 250 ms
  to 5 s instead of retrying every frame. A healthy daemon still restores every
  terminal in the first frame after reconnecting.
- **Connection establishment moved off the render thread.** The socket connect,
  the autospawn wait, and the `Hello` exchange now run on a short-lived connector
  thread; the render loop only collects the finished result. A `start`, `stop`,
  resize, or keystroke issued while disconnected now fails immediately and
  visibly ("not connected to mult-server; a connection attempt is in progress")
  instead of blocking the frame for up to 8 s, and succeeds on the next attempt
  once the background connection lands. Daemon loss is reported once per
  disconnection rather than once per retry. The synchronous request/response
  model, its idempotency keys, and scope resumption are unchanged: correlated
  waits still run on the calling thread, and the only remaining synchronous
  connects are at construction (before the first frame) and inside
  `resume_and_resend`, which must replay a request on the connection it
  re-establishes.
- **Sockets are shut down before being dropped.** Every place that dropped
  `ServerConnection` left the reader thread parked in `read_message` on its own
  duplicate of a still-open descriptor, so a thread and an fd leaked on every
  reconnect. Dropping a connection now calls `shutdown(Both)`, which releases the
  parked read; a poisoned writer lock no longer skips it.
- **`drain_events` has a per-frame budget** of 128 messages / 256 KiB of PTY
  output. A pane producing faster than the parser consumes previously kept the
  drain loop spinning until the queue emptied, so the frame never reached the
  input poll and the UI stopped responding. Leftover traffic stays queued and is
  reported to the render loop, which keeps requesting redraws until it is done.
- **A lost host terminal no longer discards unsaved state.** `terminal.draw`,
  `event::poll`, and `event::read` failures used to propagate straight out of
  `run`. Transient failures (`Interrupted`, `WouldBlock`, `TimedOut`) are now
  retried on the next tick, and a permanent one (a closed window, a dropped ssh
  session, `EIO` from a vanished pty) exits through the same path as a quit: a
  forced save checkpoint first, then the error, with the terminal guard restoring
  the TTY as before.

### Tested — hardening slice R11

- Wire-framing coverage for the cases a real `UnixStream` produces and a
  `&[u8]` reader never does: new `crates/protocol/tests/framing.rs` covers a
  truncated frame at every prefix offset (`UnexpectedEof`), a `len == 0` frame
  (`InvalidData`), a payload delivered one byte per `read`, an `Interrupted`
  read being retried, back-to-back frames decoded from one stream, an oversize
  length prefix refused before the payload is touched, and 256 seeded postcard
  mutations that must always error and never panic. The mutation generator is a
  hand-rolled xorshift64\*; no property-testing dependency was added.
- Storage decode-failure coverage: a serde *shape* error (valid JSON, current
  version, a field of the wrong type) and a corrupt-state backup whose
  `renameat` fails. Both pin today's behaviour — any shape error discards every
  workspace, and a failed rename aborts the reset and leaves the original bytes
  alone — so the leniency and user-notification work tracked as E11 has a
  regression net to change.
- Attach-replay chunk boundaries: replay is now driven at
  `RAW_HISTORY_CHUNK_BYTES - 1`, `RAW_HISTORY_CHUNK_BYTES`, `+ 1` and
  `3 × + 7`, asserting the chunk count, that no chunk exceeds the wire bound,
  that sequences stay contiguous through the watermark, and that the
  concatenated delivery equals the source byte for byte. An empty history is
  covered as the terminator case.
- Agent-backend failure paths: a spawn that fails leaves nothing marked
  running, a nonzero exit reports `Error` rather than `Done`, the pipe reader
  survives invalid UTF-8, the bounded event queue delays a reader without
  losing a byte, and a dropped consumer releases a blocked reader instead of
  pinning a thread. The test module records that this whole module is still
  unwired scaffolding, so the tests are about being safe to wire up later.
- `TranscriptJournal` gained a module-level "shipped but unwired" note listing
  what must be true before anything opens a journal. It has no production call
  site and is deliberately kept.

### Changed — hardening slice R11

- No test mutates process-global environment variables any more. Socket-path
  and config-path resolution each grew a pure seam taking the override and the
  base as parameters, with the environment-reading wrapper delegating to it;
  the fallbacks stay lazy, so an explicit override still works on a machine
  with no resolvable home directory.
- PTY integration deadlines are failure detectors rather than schedules:
  30 s for the suite, 15 s for child reaping, 20 s for the agent tests. Every
  wait returns the moment its condition holds, so the suite still completes in
  about a second. The embedded `sleep 1`/`2`/`3` fixtures were replaced by FIFO
  handshakes or keep-alive loops.
- `client_receives_scrollback_output_and_real_exit_from_server_pty` no longer
  races its own fixture. It requires live PTY output *after* the replay, but a
  command that finished before the client attached delivered everything as
  scrollback instead, so the awaited event never existed. The command now
  blocks on a FIFO that the test opens only once create-and-attach has
  returned, making the second write live output by construction.

### Fixed — hardening slice R11

- CI now proves the PTY integration tests actually ran. The `nix` job must skip
  them (its sandbox cannot allocate PTYs) and reported 24 tests in 0.00 s while
  nothing anywhere asserted otherwise. `MULT_REQUIRE_PTY_INTEGRATION=1` turns a
  requested skip into a failure, and the suite prints an execution sentinel only
  after a real daemon has driven a real PTY; the `cargo` job sets the variable,
  runs the suite with `--nocapture`, and greps for both the sentinel and a
  plausible passed count.
- The Nix derivation now sets `SHELL` as well as `MULT_TEST_SHELL`. Only the
  (skipped) integration harness read the latter, while the code that needs a
  shell in the sandbox reads `$SHELL` and falls back to a `/bin/sh` that does
  not exist there — a trap the next test to spawn a pane would have hit.

### Fixed — idle cost and save discipline (R3)

- **A visible pane is resized only when its size changed.** Both resize sites
  (terminal and chat agent) computed a `changed` flag for their return value and
  then called `resize` unconditionally, so an idle session serialized and wrote
  a `Resize` to the socket about 125 times a second from two sites — each one
  costing the daemon a pane lock, a master lock and a `TIOCSWINSZ` to set the
  size the pane already had. Both now gate the call, so idle writes nothing.
- **Agent status journals are polled on a timer, with cached paths.** The status
  bridge ran on every ~16 ms tick per agent chat: a `format!`, two `join`s and a
  `PathBuf` clone (four allocations) followed by
  `open`+`fstat`+`seek`+`read`+`close`. With four chats that was ~1250 syscalls
  and ~1000 allocations a second at complete idle. The journal path is now built
  once per agent session and cached (keyed by chat, invalidated by a new
  generation), and the journals are read every 250 ms instead of every tick — a
  16× reduction with no perceptible change to the status dot.
- **An unchanged git branch no longer forces a redraw.** The two-second branch
  refresh returns whether anything changed, but the caller discarded it and set
  `needs_redraw` unconditionally, defeating the client's own redraw gating twice
  a minute on a completely idle session. The return value is now used.
- **State saves are rate-limited and go through the locked store only.** Two
  fixes to one path: saves were being made through the free `storage::save`
  while `main` held a lock-holding `StateStore`, so there were two write paths
  and only one held the ownership lock; and `save_if_dirty_with` ran on every
  tick while dirty, doing a full `to_string_pretty` of the project plus
  `sync_all`, a rename and a directory `sync` — once per streamed agent delta.
  The event loop now saves exclusively through the store `main` locked (the
  thread-local side channel that used to bridge the two is gone, and the free
  `storage::save` acquires the same lock for its whole write), and ordinary
  content saves are spaced at least one second apart. Quitting, a signal, a
  fatal host-terminal error, an agent launch, and any structural change (a
  workspace, chat or terminal added or removed) still save immediately, so
  nothing is lost on exit.
- **A chat message can no longer grow without bound.** Streamed deltas append to
  the last message and nothing terminates it, so one runaway agent could grow a
  single `String` until the process died — and every save re-serialized all of
  it. A message is now capped at 64 KiB and marked `[truncated]`; further deltas
  are dropped *and* reported as no change, so a full message stops provoking
  saves as well as stopping the growth. This path is Phase 3.4 scaffolding with
  no production caller today, so the cap is about being safe to wire up.
- **Generated runtime artifacts no longer take a blocking lock on the render
  thread.** `write_private_runtime_file` took `flock(LOCK_EX)` on a fixed path,
  so a second `mult` instance starting an agent stalled the first one's event
  loop for as long as it held the lock. The lock is now `LOCK_EX|LOCK_NB` with a
  bounded retry (5 × 2 ms); contention that outlasts that degrades to starting
  the agent without its status extension instead of freezing the UI.
- The git probe (D5) needed nothing further: R4 already replaced the `git -C`
  fork with a direct, size-capped, `O_NOFOLLOW` read of `.git/HEAD`, so the
  two-second refresh is now a couple of syscalls per workspace on the UI thread
  rather than a process spawn.

### Fixed — emulator panics and fuzzing (R5)

- **A pane one row or one column in size no longer panics the terminal
  emulator.** `fnug-vt100` subtracts from a screen dimension without checking
  it, so a 1×N or N×1 pane aborted on input as ordinary as a stray non-UTF-8
  byte, an emoji or a line feed — in debug it panicked, in release the wrapped
  arithmetic corrupted the grid, and either way it took down a UI holding the
  terminal in raw mode. The safe floor was established empirically at 2×2 (every
  smaller size fails on at least one ordinary byte class; 2×2 and up survived the
  hand corpus and 12 000 seeded streams) and is now enforced in one place,
  `bounded_screen_dimensions`, so the client, the daemon and the wire agree and
  no `.max(1)` call site can undercut it. A pane too small to hold a screen now
  renders a visible "too small" marker instead of a crop of a differently sized
  terminal (A13).
- **Narrowing a pane that holds a double-width character no longer panics.**
  `fnug-vt100` truncates a row on resize without clearing the orphaned "wide"
  flag on the cell that becomes the last one, so the next character printed
  there unwrapped `None` — reachable by dragging a pane one column narrower with
  any CJK text or emoji on screen. The split character is now erased before the
  resize, which destroys nothing the truncation was not already destroying, and
  the work is skipped unless a row actually ends in one. Known residual: the
  emulator resizes its alternate grid too and only the current grid is reachable
  through its public API, so a pane narrowed while in the alternate screen can
  still carry a split character back to the normal grid on exit (A14).
- Both of the above are upstream defects in `fnug-vt100` 0.15.2; the backlog
  rows record them so the notes survive a dependency bump.

### Performance — emulator feed (R5)

- **Escape sequences are no longer fed to the emulator one byte at a time.**
  Only the printable run was batched, so for escape-dense output — any TUI child,
  which is the primary use case — most bytes paid a full `vte` dispatch setup on
  a one-byte slice. The query detector's state machine is now independent of the
  screen, which lets the feed scan a whole escape sequence ahead and hand it to
  the parser in one call. The behaviour that forced the per-byte feed is
  preserved exactly: batching stops at every sequence boundary, and the only
  sequences that produce an answer are queries, which never move the cursor, so
  a cursor position report still describes the screen at the query point (D7).
- **Entering a CSI sequence no longer allocates.** The detector held its
  parameter bytes in a `Vec` built on entry to every CSI; a TUI child redrawing
  emits thousands per frame. The 128-byte bound already existed, so it is now an
  inline array plus a length held across states (D6).

### Tested — emulator panics and fuzzing (R5)

- **The batched feed's "behaviourally identical to feeding every byte
  individually" claim is now tested.** It is compared against an explicit
  byte-at-a-time reference on screen contents, cursor position, scrollback depth
  and the emitted response bytes, over 26 hand-written adversarial cases (split
  and truncated CSI, oversized CSI, `ESC` at the end of a chunk, CPR mid-stream,
  OSC/DCS, mixed and invalid UTF-8) at four screen sizes and eight chunk sizes,
  plus 64 seeded xorshift streams at five chunk sizes. No `proptest`: a failure
  reproduces from its printed seed. Both a naive and a D7-shaped batching bug
  were injected to confirm the test catches them (G4).
- **Fuzz targets are back**, in a top-level `fuzz/` that is its own Cargo
  workspace, so normal builds, `cargo deny` and the MSRV job are unaffected and
  nothing is added to CI. `protocol_read_message` drives an arbitrary byte
  stream through the framing reader in both directions; `vt_response_detector`
  drives arbitrary output through the query detector and the emulator with
  interleaved resizes at input-chosen pane sizes, including the clamped floor —
  it reproduces A13 in under a minute with the floor removed. Running them is
  documented in `CONTRIBUTING.md` (G3).
- Boundary regression tests cover 0×0, 1×1, 1×N, N×1 and 2×2 across ten byte
  classes on every public route into a parser, and the narrowing panic is pinned
  across two glyph widths and every shrink step down to the floor.

### Added — CLI, config validation and state recovery (R7a)

- **Both binaries parse their arguments.** `mult --help`, `mult --version`,
  `mult -h` and `mult -V` used to start the TUI, and a mistyped `--socekt` used
  to start it too, silently talking to the wrong daemon. A new `src/cli.rs`
  parses `--help`/`-h`, `--version`/`-V`, and `--config`/`--state`/`--socket`
  (both `--flag value` and `--flag=value`; values are split on bytes, so a path
  that is not valid UTF-8 survives). An unrecognized option, a stray positional,
  a missing value or a repeated flag prints a message plus a `--help` pointer and
  exits `2`. `mult-server` accepts `--socket` and **rejects** `--config` and
  `--state` rather than accepting a flag it would ignore. Hand-rolled rather than
  adding `clap`, per the workspace's no-new-dependencies rule. `default-run` is
  set, so a bare `cargo run` means the client (E1).
- **Every path option has one precedence rule: flag > environment > default.**
  Implemented through the existing pure path seams — `config_path_from` gained a
  flag argument, `StatePaths::resolve_with` was added, and `default_socket_path`
  consults a set-once `OnceLock` the CLI installs before any thread exists (the
  client resolves its socket deep inside `PtyRuntime`, and mutating
  `$MULT_SOCKET_PATH` at runtime was not an option). Documented in `README.md`,
  `docs/CONFIG.md` and both `--help` texts (E1).
- **`Config::warnings() -> &[String]`** and **`LoadedState::notice:
  Option<String>`**: the two seams through which non-fatal configuration
  problems and state recovery reach the user. Both are printed to stderr at
  startup — where they are legible before the alternate screen opens — and both
  feed the in-app notice surface (E6, E11).

### Changed — configuration is validated (R7a)

- **Unknown config keys are now a startup error.** `deny_unknown_fields` applies
  to the top-level object, to `colorscheme`, and to a `projects` entry, so a typo
  like `auto_start_terminal` (no `s`) or `colorscheme.foreground` fails with the
  list of accepted keys instead of being accepted and doing nothing. A `projects`
  entry is deserialized by a hand-written visitor rather than an `#[serde(untagged)]`
  enum, which reported every failure as "data did not match any variant" and lost
  the position (E6).
- **A colour that does not parse, and a `projects` path that is not a directory,
  are warnings rather than errors.** The colour keeps its built-in default and
  the shortcut stays in the list; both are reported by key and by value. The
  policy — anything that means the file cannot be understood is an error,
  anything with a safe fallback is a warning — is written into `docs/CONFIG.md`
  (E6).
- **State decoding is lenient about what it can rebuild.** A file that lost part
  of itself — a renamed key, a `null` where an array belonged, missing
  `next_*_id` hints, an identity table that no longer matches the sessions — now
  loads everything that decoded. The allocator hints and the identity and
  generation tables are reconciled against the sessions that survived: stale
  entries are dropped and missing tokens are minted fresh, which is the fail-safe
  direction because a fresh token cannot claim an existing daemon session (the
  pane comes back stopped). IDs, names and the state namespace stay required, and
  a value of the wrong *type* is still an error rather than data silently
  discarded. The V1→V2 migration and the future-version byte-preservation
  guarantee are unchanged and pinned by a new test (E11).

### Fixed — error surfacing (R7a)

- **A bad config no longer `Debug`-dumps.** Both binaries return `ExitCode` and
  print errors with `Display`. A parse failure reads
  `config error at <path>:<line>:<col>: <message>`, with serde's redundant
  trailing " at line N column M" stripped (E5).
- **A symlinked config says so.** The hardened `O_NOFOLLOW` read introduced in
  R4 failed inside `openat`, before any of `mult`'s own descriptions applied, so
  the user saw a bare
  `Error: Os { code: 40, kind: FilesystemLoop, message: "Too many levels of symbolic links" }`
  that never mentioned the config file. `ELOOP`/`ENOTDIR` now produce a message
  naming the path, stating that symlinked config files are not supported and why,
  and giving the workaround (copy the file, or point `--config` /
  `$MULT_CONFIG_PATH` at a real file) — the layout most dotfile managers produce
  (E5).
- **Config directory failures say "config".** The reader shared with state
  emitted `state parent …` and `private state directory …` even when it was the
  config directory that failed; the messages are parameterized by purpose now
  (E5).
- **A state reset is no longer silent.** When a decode failure genuinely forces
  backup-and-reset, the notice names the file, the decode error and the
  `.corrupt-*` backup, instead of leaving a file nobody was told about as the
  only evidence that every workspace was discarded (E11).
- Config is loaded **before** the state lock is taken, so `mult --config
  <broken>` reports the config error rather than whatever the state path had to
  say (E5).

### Tested — CLI and configuration (R7a)

- 12 tests for the argument parser, driving the pure `parse` function rather than
  spawning processes: separated and inline values, a non-UTF-8 value, help and
  version for both binaries, unknown flags, stray positionals, `--`, missing and
  repeated values, and the daemon's rejection of client-only options (E1).
- `config.rs` grew from 9 tests to 23, covering malformed JSON, unknown keys at
  all three levels, invalid colour strings, a project path that does not exist, a
  symlinked config, a config directory rejection naming "config", and flag >
  environment > default precedence (G13).
- `storage.rs` gained `a_partially_unknown_state_keeps_everything_it_can_decode`
  and `leniency_does_not_weaken_the_version_boundary`;
  `a_shape_error_backs_the_file_up_and_resets_to_a_fresh_state` became
  `…_and_reports_where_the_backup_went` and asserts the notice. The
  backup-rename-failure test is unchanged — that path correctly aborts and
  preserves the original bytes. `model.rs` pins the reconciliation of the
  identity tables and the missing/`null`/wrong-type decode boundary (E11).

### Added — status surface and in-app affordances (R7b)

- **A dismissible status surface.** Errors that belong to no pane had nowhere to
  go: a daemon that would not connect, a protocol mismatch, a failed state save
  and the connection-wide `ServerMessage::Error` were all attributed to
  `PtyKey::Terminal(TerminalId(0))` — an id that cannot exist — so the
  explanation was written into a pane nobody could open and the user got an inert
  UI with no reason for it. `App` now carries up to four notices with a 12-second
  lifetime, de-duplicated so a failure that repeats every frame refreshes its row
  instead of stacking copies, and dropped oldest-first. The surface occupies rows
  only while it has something to say. `Ctrl+n` (and a "Dismiss notices" palette
  entry) clears it; `Ctrl+n` is only consumed when there is something to dismiss,
  so it still reaches a PTY on a quiet session. A save failure is sticky, because
  it describes a condition that is still true, and is retracted by the next
  successful save (E2, B8).
- **`?` / `F1` opens a keybinding overlay** generated from the same
  `app::BINDINGS` table the command palette filters, so a binding cannot be added
  to one and forgotten in the other. `?` only opens it when no pane would have
  received the key — a selected chat or terminal takes every ordinary character,
  which is how a PTY is started and typed at — while `F1` and the palette entry
  are unconditional. Any key closes it, and it is drawn over the frame rather
  than in the layout, so it costs nothing while it is down (E4).
- **"Reload config" in the command palette.** `config.json` is re-read in place;
  the colorscheme, `projects`, agent commands and auto-start apply to the next
  frame. The notice states what did *not* change — `mouse_capture` is a terminal
  mode set once per session, and an already-running PTY keeps the command it was
  started with — so an applied reload never looks like a no-op. A config that
  fails to load leaves the running one in place and reports the error rather than
  ending a session holding live PTYs over a colorscheme typo (E9).
- **`NO_COLOR` is honoured.** Set to any non-empty value, every colour becomes
  `Color::Reset`; emphasis that would have been a background colour becomes
  reverse video (E10).

### Changed — prompts and status glyphs (R7b)

- **Prompts have a cursor.** Text entry was append-only, in four duplicated
  copies. One `PromptInput` now owns the text for every prompt and supports
  `Left`/`Right`, `Home`/`End`, `Ctrl+a`/`Ctrl+e`, `Backspace`, `Delete`,
  `Ctrl+w` and `Ctrl+u`. The cursor is counted in characters with byte offsets
  derived on demand, so no edit can land on a non-char boundary, and the renderer
  styles the character under the cursor rather than replacing it — a wide or
  combining character keeps its own display column (E7, F13).
- **Chat state is carried by shape, not only by hue.** Every chat rendered the
  identical `"● "`, so a red-green colourblind user could not tell a finished
  agent from a failed one, and under `NO_COLOR` nobody could. Six single-width,
  long-standing Unicode glyphs now carry the state — no Nerd Font or emoji font
  needed — and the sidebar's selected-row highlight sets a background only, never
  a foreground, so selecting a row cannot put its state back on colour alone
  (E8).
- **Chat search says it is not wired up.** `Ctrl+s` on a chat searches the
  structured transcript, which only the experimental process-agent backend writes
  and which has no call path, so it is empty for every chat that can be created
  and "No matches." read as "your text is not in this chat". The pane now says
  the transcript is not populated and that chats run in a PTY. Nothing was
  deleted: `src/transcript.rs` builds on those types (E12).
- **The list selection wraps modularly.** `index.checked_sub(delta).unwrap_or(len
  - delta)` was only right for `delta == 1` and underflowed for `delta > len`; a
  single `ListSelection` using `rem_euclid` replaces four copies of it (F21,
  F13).

### Fixed — help overlay legibility (R7b, finishing pass)

- The overlay was capped at 64 columns whatever the terminal was, so its longest
  labels were clipped mid-word by the renderer — "Move through results (palette,
  projec" — with nothing on screen marking the cut, and being borderless it ran
  straight into the pane it covered ("▣ websKeybindings"). It is now a bordered
  panel sized to its widest row and no wider than the frame, with any label that
  still will not fit ellipsized. Vertical truncation is unchanged: rows past the
  bottom are dropped and the last row says so.

### Tested — status surface and affordances (R7b)

- Three new `insta` snapshots pin whole frames as a glyph grid plus a per-cell
  style grid: `snapshot_help_overlay`, `snapshot_status_surface_with_an_error`
  (an error, a warning and a sticky save failure at once) and
  `snapshot_no_color_frame`, whose style legend — every entry `Color::Reset` — is
  the assertion. `snapshot_command_palette_prompt` gained the "Show keybindings"
  row at the head of the list.
- `prompt_motions_and_kills_work_in_every_text_prompt` drives each prompt through
  its own handler exactly as `handle_key` dispatches it, so the shared classifier
  is asserted rather than assumed; the notice TTL and cap tests inject the clock,
  so nothing waits on wall time.

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
